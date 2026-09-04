//! Advisory Git hook integration for the colocated Atomic bridge.
//!
//! Atomic installs only an explicitly owned `post-checkout` dispatcher. The
//! dispatcher appends immutable local evidence and schedules a read-only
//! observation after Git exits; it never imports, reconciles, materializes, or
//! moves refs. Unmanaged hook systems and custom `core.hooksPath` values are
//! detected and left untouched.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::Duration;

use chrono::Utc;
use clap::{Args, Subcommand};
use git2::{ErrorCode, Repository as GitRepository};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::observation::{observe_git, GitObservation, HeadObservation};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

const DISPATCHER_MARKER: &str = "# atomic:git-bridge-dispatcher:v1";
const LEGACY_MARKER_BEGIN: &str = "# atomic:git:begin";
const LEGACY_MARKER_END: &str = "# atomic:git:end";
const EVENT_VERSION: u32 = 1;
const EVENT_JOURNAL_RELATIVE: &str = ".atomic/bridge/git-events.jsonl";
const DEFERRED_DIRECTORY_RELATIVE: &str = ".atomic/bridge/deferred-observations";
const DEFERRED_DELAY_MS: u64 = 300;

/// Backward-compatible entry point for bridge hook management.
#[derive(Debug, Args)]
pub struct Hooks {
    #[command(subcommand)]
    pub command: HookCommands,
}

#[derive(Debug, Subcommand)]
pub enum HookCommands {
    /// Enable the advisory bridge `post-checkout` dispatcher.
    Install,
    /// Remove an Atomic-owned bridge dispatcher.
    Uninstall,
    /// Show advisory bridge hook status.
    Status,
}

impl Command for Hooks {
    fn run(&self) -> CliResult<()> {
        let root = find_repository_root()?;
        match &self.command {
            HookCommands::Install => enable_bridge(&root),
            HookCommands::Uninstall => uninstall_bridge(&root),
            HookCommands::Status => show_status(&root),
        }
    }
}

#[derive(Debug)]
struct GitHookContext {
    common_dir: PathBuf,
    custom_hooks_path: Option<PathBuf>,
}

#[derive(Debug, Eq, PartialEq)]
enum DispatcherInstall {
    Installed,
    Refreshed,
    Unmanaged,
}

/// Enable the bridge's advisory `post-checkout` integration.
///
/// Only a missing hook or an exact Atomic-owned dispatcher is written. A
/// custom `core.hooksPath`, symlink, binary, or unmanaged hook is reported with
/// an integration command and left byte-for-byte untouched.
pub(crate) fn enable_bridge(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let binary = std::env::current_exe()
        .map_err(|error| git_error(format!("cannot resolve the Atomic binary path: {error}")))?;
    let context = git_hook_context(&root)?;

    if let Some(custom_hooks_path) = context.custom_hooks_path {
        print_unmanaged_instructions(
            &format!(
                "core.hooksPath is configured as '{}'; Atomic did not modify that hook system",
                custom_hooks_path.display()
            ),
            &binary,
        )?;
        return Ok(());
    }

    let hooks_dir = context.common_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).map_err(|error| {
        git_error(format!(
            "cannot create Git hooks directory '{}': {error}",
            hooks_dir.display()
        ))
    })?;

    migrate_legacy_import_hooks(&hooks_dir, &binary)?;

    let hook_path = hooks_dir.join("post-checkout");
    let script = dispatcher_script(&binary).map_err(|error| {
        git_error(format!(
            "cannot build the advisory post-checkout dispatcher: {error}"
        ))
    })?;
    match install_or_refresh_dispatcher(&hook_path, &script).map_err(|error| {
        git_error(format!(
            "cannot install advisory dispatcher '{}': {error}",
            hook_path.display()
        ))
    })? {
        DispatcherInstall::Installed => print_info(&format!(
            "Installed Atomic advisory post-checkout dispatcher at {}",
            hook_path.display()
        )),
        DispatcherInstall::Refreshed => print_info(&format!(
            "Refreshed Atomic advisory post-checkout dispatcher at {}",
            hook_path.display()
        )),
        DispatcherInstall::Unmanaged => print_unmanaged_instructions(
            &format!(
                "existing post-checkout hook '{}' is not Atomic-owned and was left untouched",
                hook_path.display()
            ),
            &binary,
        )?,
    }

    Ok(())
}

fn git_hook_context(root: &Path) -> CliResult<GitHookContext> {
    let common_dir = match observe_git(root)
        .map_err(|error| git_error(format!("cannot resolve Git administrative paths: {error}")))?
    {
        GitObservation::NoGit { .. } => {
            return Err(git_error(
                "cannot enable the bridge outside a Git repository",
            ));
        }
        GitObservation::Repository(observation) => {
            if observation.paths.worktree_root.is_none() {
                return Err(git_error(
                    "cannot enable a working-copy hook for a bare Git repository",
                ));
            }
            observation.paths.common_dir.clone()
        }
    };

    let repository = GitRepository::open(root)
        .map_err(|error| git_error(format!("cannot open Git repository: {error}")))?;
    let config = repository
        .config()
        .map_err(|error| git_error(format!("cannot read Git configuration: {error}")))?;
    let custom_hooks_path = match config.get_path("core.hooksPath") {
        Ok(path) => Some(path),
        Err(error) if error.code() == ErrorCode::NotFound => None,
        Err(error) => {
            return Err(git_error(format!(
                "cannot read configured core.hooksPath: {error}"
            )))
        }
    };

    Ok(GitHookContext {
        common_dir,
        custom_hooks_path,
    })
}

fn dispatcher_script(binary: &Path) -> io::Result<String> {
    let binary = shell_quote(binary)?;
    Ok(format!(
        "#!/bin/sh\n{DISPATCHER_MARKER}\n\
{binary} git bridge hook-post-checkout \"$@\" || true\n\
if [ -d \"$0.d\" ]; then\n\
  for atomic_bridge_hook in \"$0.d\"/*; do\n\
    [ -f \"$atomic_bridge_hook\" ] && [ -x \"$atomic_bridge_hook\" ] || continue\n\
    \"$atomic_bridge_hook\" \"$@\" || true\n\
  done\n\
fi\n\
exit 0\n"
    ))
}

fn integration_command(binary: &Path) -> io::Result<String> {
    Ok(format!(
        "{} git bridge hook-post-checkout \"$@\" || true",
        shell_quote(binary)?
    ))
}

fn shell_quote(path: &Path) -> io::Result<String> {
    let value = path.to_str().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidData,
            "the Atomic binary path is not valid UTF-8 and cannot be embedded in a Git hook",
        )
    })?;
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

fn print_unmanaged_instructions(reason: &str, binary: &Path) -> CliResult<()> {
    let command = integration_command(binary).map_err(|error| {
        git_error(format!(
            "cannot format hook integration instructions: {error}"
        ))
    })?;
    print_warning(reason);
    print_info("Add this advisory command to the existing post-checkout hook or hook manager:");
    print_info(&format!("  {command}"));
    Ok(())
}

fn install_or_refresh_dispatcher(path: &Path, script: &str) -> io::Result<DispatcherInstall> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    match metadata {
        None => {
            write_new_executable(path, script.as_bytes())?;
            Ok(DispatcherInstall::Installed)
        }
        Some(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Ok(DispatcherInstall::Unmanaged)
        }
        Some(_) => {
            let existing = fs::read(path)?;
            if is_owned_dispatcher(&existing) || is_legacy_atomic_only(&existing) {
                replace_owned_executable(path, script.as_bytes())?;
                Ok(DispatcherInstall::Refreshed)
            } else {
                Ok(DispatcherInstall::Unmanaged)
            }
        }
    }
}

fn is_owned_dispatcher(content: &[u8]) -> bool {
    let expected = format!("#!/bin/sh\n{DISPATCHER_MARKER}\n");
    content.starts_with(expected.as_bytes())
}

fn is_legacy_atomic_only(content: &[u8]) -> bool {
    let Ok(content) = std::str::from_utf8(content) else {
        return false;
    };
    let mut in_atomic_section = false;
    let mut saw_begin = false;
    let mut saw_end = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == LEGACY_MARKER_BEGIN && !in_atomic_section && !saw_begin {
            in_atomic_section = true;
            saw_begin = true;
            continue;
        }
        if trimmed == LEGACY_MARKER_END && in_atomic_section {
            in_atomic_section = false;
            saw_end = true;
            continue;
        }
        if !in_atomic_section && !trimmed.is_empty() && trimmed != "#!/bin/sh" {
            return false;
        }
    }

    saw_begin && saw_end && !in_atomic_section
}

fn migrate_legacy_import_hooks(hooks_dir: &Path, binary: &Path) -> CliResult<()> {
    for hook_name in ["post-commit", "post-merge", "post-rewrite"] {
        let path = hooks_dir.join(hook_name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(git_error(format!(
                    "cannot inspect legacy hook '{}': {error}",
                    path.display()
                )))
            }
        };
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            continue;
        }
        let content = fs::read(&path).map_err(|error| {
            git_error(format!(
                "cannot inspect legacy hook '{}': {error}",
                path.display()
            ))
        })?;
        if is_legacy_atomic_only(&content) {
            fs::remove_file(&path).map_err(|error| {
                git_error(format!(
                    "cannot remove Atomic-owned legacy hook '{}': {error}",
                    path.display()
                ))
            })?;
            print_info(&format!(
                "Removed Atomic-owned legacy synchronous hook {}",
                path.display()
            ));
        } else if content
            .windows(LEGACY_MARKER_BEGIN.len())
            .any(|window| window == LEGACY_MARKER_BEGIN.as_bytes())
        {
            print_unmanaged_instructions(
                &format!(
                    "legacy Atomic section is mixed with unmanaged hook content in '{}'; the file was left untouched and the legacy import section must be removed manually",
                    path.display()
                ),
                binary,
            )?;
        }
    }
    Ok(())
}

fn write_new_executable(path: &Path, content: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    set_executable(path)?;
    Ok(())
}

fn replace_owned_executable(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(ErrorKind::InvalidInput, "hook path has no parent directory")
    })?;
    let temporary = parent.join(format!(".atomic-post-checkout-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        write_new_executable(&temporary, content)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn uninstall_bridge(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let binary = std::env::current_exe()
        .map_err(|error| git_error(format!("cannot resolve the Atomic binary path: {error}")))?;
    let context = git_hook_context(&root)?;
    if let Some(custom_hooks_path) = context.custom_hooks_path {
        print_unmanaged_instructions(
            &format!(
                "core.hooksPath is configured as '{}'; Atomic did not modify that hook system",
                custom_hooks_path.display()
            ),
            &binary,
        )?;
        return Ok(());
    }

    let path = context.common_dir.join("hooks/post-checkout");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            print_info("No Atomic advisory post-checkout dispatcher is installed.");
            return Ok(());
        }
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect hook '{}': {error}",
                path.display()
            )))
        }
    };

    if metadata.file_type().is_file() {
        let content = fs::read(&path).map_err(|error| {
            git_error(format!("cannot read hook '{}': {error}", path.display()))
        })?;
        if is_owned_dispatcher(&content) || is_legacy_atomic_only(&content) {
            fs::remove_file(&path).map_err(|error| {
                git_error(format!("cannot remove hook '{}': {error}", path.display()))
            })?;
            print_info("Removed the Atomic advisory post-checkout dispatcher.");
            return Ok(());
        }
    }

    print_unmanaged_instructions(
        &format!(
            "post-checkout hook '{}' is not Atomic-owned and was left untouched",
            path.display()
        ),
        &binary,
    )
}

fn show_status(root: &Path) -> CliResult<()> {
    let root = canonical_root(root)?;
    let context = git_hook_context(&root)?;
    if let Some(custom_hooks_path) = context.custom_hooks_path {
        print_info(&format!(
            "post-checkout: externally managed through core.hooksPath '{}'",
            custom_hooks_path.display()
        ));
        return Ok(());
    }

    let path = context.common_dir.join("hooks/post-checkout");
    let status = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => "not installed",
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect hook '{}': {error}",
                path.display()
            )))
        }
        Ok(metadata) if metadata.file_type().is_symlink() => "unmanaged symlink",
        Ok(metadata) if !metadata.file_type().is_file() => "unmanaged non-file",
        Ok(_) => match fs::read(&path) {
            Ok(content) if is_owned_dispatcher(&content) => "Atomic-owned dispatcher installed",
            Ok(_) => "unmanaged hook installed",
            Err(_) => "unmanaged unreadable hook",
        },
    };
    print_info(&format!("post-checkout: {status} ({})", path.display()));
    Ok(())
}

#[derive(Debug, Serialize)]
struct CheckoutEventEvidence {
    version: u32,
    record_type: &'static str,
    event_id: String,
    recorded_at: String,
    advisory: bool,
    old_head: String,
    new_head: String,
    checkout_kind: &'static str,
    worktree_root: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct DeferredObservationRequest {
    version: u32,
    request_type: String,
    event_id: String,
    requested_at: String,
    worktree_root: PathBuf,
}

#[derive(Debug)]
struct ScheduledObservation {
    event_id: String,
    request_path: PathBuf,
}

#[derive(Debug, Serialize)]
struct DeferredObservationReceipt {
    version: u32,
    record_type: &'static str,
    receipt_id: String,
    cause_event_id: String,
    recorded_at: String,
    advisory: bool,
    observation: GitObservationEvidence,
}

#[derive(Debug, Serialize)]
struct GitObservationEvidence {
    git_present: bool,
    worktree_root: Option<PathBuf>,
    worktree_git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
    head_kind: Option<&'static str>,
    head_symref: Option<String>,
    head_oid: Option<String>,
    head_tree_oid: Option<String>,
    index_tree_oid: Option<String>,
    index_digest: Option<String>,
    refs_digest: Option<String>,
    index_locked: bool,
    ref_locks: Vec<PathBuf>,
    operation_state: Option<String>,
    operation_markers: Vec<&'static str>,
}

/// Append checkout evidence and schedule a separate read-only observer.
pub(crate) fn record_post_checkout(
    root: &Path,
    old_head: &str,
    new_head: &str,
    checkout_flag: &str,
) -> CliResult<()> {
    let scheduled = persist_checkout_event(root, old_head, new_head, checkout_flag)?;
    warn_if_view_mismatch(root);
    spawn_deferred_observer(root, &scheduled)
}

fn persist_checkout_event(
    root: &Path,
    old_head: &str,
    new_head: &str,
    checkout_flag: &str,
) -> CliResult<ScheduledObservation> {
    validate_hook_oid("old HEAD", old_head)?;
    validate_hook_oid("new HEAD", new_head)?;
    let checkout_kind = match checkout_flag {
        "0" => "file",
        "1" => "branch",
        value => {
            return Err(git_error(format!(
                "post-checkout flag must be 0 or 1, got '{value}'"
            )))
        }
    };
    let root = canonical_root(root)?;
    let event_id = Uuid::new_v4().to_string();
    let recorded_at = Utc::now().to_rfc3339();
    let event = CheckoutEventEvidence {
        version: EVENT_VERSION,
        record_type: "post-checkout",
        event_id: event_id.clone(),
        recorded_at: recorded_at.clone(),
        advisory: true,
        old_head: old_head.to_ascii_lowercase(),
        new_head: new_head.to_ascii_lowercase(),
        checkout_kind,
        worktree_root: root.clone(),
    };
    append_journal_record(&root, &event)?;

    let deferred_dir = root.join(DEFERRED_DIRECTORY_RELATIVE);
    fs::create_dir_all(&deferred_dir).map_err(|error| {
        git_error(format!(
            "cannot create deferred-observation directory '{}': {error}",
            deferred_dir.display()
        ))
    })?;
    let request_path = deferred_dir.join(format!("{event_id}.json"));
    let request = DeferredObservationRequest {
        version: EVENT_VERSION,
        request_type: "deferred-git-observation".to_string(),
        event_id: event_id.clone(),
        requested_at: recorded_at,
        worktree_root: root,
    };
    write_immutable_json(&request_path, &request)?;

    Ok(ScheduledObservation {
        event_id,
        request_path,
    })
}

fn validate_hook_oid(label: &str, value: &str) -> CliResult<()> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(git_error(format!(
            "{label} from post-checkout is not a hexadecimal Git object ID"
        )));
    }
    Ok(())
}

fn append_journal_record<T: Serialize>(root: &Path, record: &T) -> CliResult<()> {
    let path = root.join(EVENT_JOURNAL_RELATIVE);
    let parent = path
        .parent()
        .ok_or_else(|| git_error("event journal has no parent directory"))?;
    fs::create_dir_all(parent).map_err(|error| {
        git_error(format!(
            "cannot create event journal directory '{}': {error}",
            parent.display()
        ))
    })?;
    let mut line = serde_json::to_vec(record)
        .map_err(|error| git_error(format!("cannot encode Git event evidence: {error}")))?;
    line.push(b'\n');

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| {
            git_error(format!(
                "cannot open append-only event journal '{}': {error}",
                path.display()
            ))
        })?;
    file.write_all(&line).map_err(|error| {
        git_error(format!(
            "cannot append event evidence to '{}': {error}",
            path.display()
        ))
    })?;
    file.sync_data().map_err(|error| {
        git_error(format!(
            "cannot sync event evidence in '{}': {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn write_immutable_json<T: Serialize>(path: &Path, value: &T) -> CliResult<()> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| git_error(format!("cannot encode deferred request: {error}")))?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            git_error(format!(
                "cannot create immutable deferred request '{}': {error}",
                path.display()
            ))
        })?;
    file.write_all(&bytes).map_err(|error| {
        git_error(format!(
            "cannot write deferred request '{}': {error}",
            path.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        git_error(format!(
            "cannot sync deferred request '{}': {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn spawn_deferred_observer(root: &Path, scheduled: &ScheduledObservation) -> CliResult<()> {
    let binary = std::env::current_exe()
        .map_err(|error| git_error(format!("cannot resolve the Atomic binary path: {error}")))?;
    ProcessCommand::new(binary)
        .arg("git")
        .arg("bridge")
        .arg("observe-deferred")
        .arg("--root")
        .arg(root)
        .arg("--request")
        .arg(&scheduled.request_path)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            git_error(format!(
                "checkout event {} was journaled but its deferred observer could not be started: {error}",
                scheduled.event_id
            ))
        })?;
    Ok(())
}

/// Execute one deferred request using the existing strictly read-only observer.
pub(crate) fn run_deferred_observation(root: &Path, request_path: &Path) -> CliResult<()> {
    thread::sleep(Duration::from_millis(DEFERRED_DELAY_MS));

    let root = canonical_root(root)?;
    let deferred_dir = root.join(DEFERRED_DIRECTORY_RELATIVE);
    let canonical_deferred_dir = fs::canonicalize(&deferred_dir).map_err(|error| {
        git_error(format!(
            "cannot resolve deferred-observation directory '{}': {error}",
            deferred_dir.display()
        ))
    })?;
    let canonical_request = fs::canonicalize(request_path).map_err(|error| {
        git_error(format!(
            "cannot resolve deferred request '{}': {error}",
            request_path.display()
        ))
    })?;
    if canonical_request.parent() != Some(canonical_deferred_dir.as_path()) {
        return Err(git_error(format!(
            "refusing deferred request outside '{}': {}",
            canonical_deferred_dir.display(),
            canonical_request.display()
        )));
    }

    let bytes = fs::read(&canonical_request).map_err(|error| {
        git_error(format!(
            "cannot read deferred request '{}': {error}",
            canonical_request.display()
        ))
    })?;
    let request: DeferredObservationRequest = serde_json::from_slice(&bytes).map_err(|error| {
        git_error(format!(
            "deferred request '{}' is malformed: {error}",
            canonical_request.display()
        ))
    })?;
    let expected_file_name = format!("{}.json", request.event_id);
    if request.version != EVENT_VERSION
        || request.request_type != "deferred-git-observation"
        || request.worktree_root != root
        || canonical_request.file_name().and_then(|name| name.to_str())
            != Some(expected_file_name.as_str())
    {
        return Err(git_error(format!(
            "deferred request '{}' does not match its immutable identity",
            canonical_request.display()
        )));
    }

    let observation = observe_git(&root)
        .map_err(|error| git_error(format!("deferred Git observation failed: {error}")))?;
    let receipt = DeferredObservationReceipt {
        version: EVENT_VERSION,
        record_type: "deferred-observation",
        receipt_id: format!("{}:observation", request.event_id),
        cause_event_id: request.event_id,
        recorded_at: Utc::now().to_rfc3339(),
        advisory: true,
        observation: observation_evidence(observation),
    };
    append_journal_record(&root, &receipt)?;
    fs::remove_file(&canonical_request).map_err(|error| {
        git_error(format!(
            "observation receipt was appended but request '{}' could not be consumed: {error}",
            canonical_request.display()
        ))
    })?;
    Ok(())
}

fn observation_evidence(observation: GitObservation) -> GitObservationEvidence {
    match observation {
        GitObservation::NoGit { root } => GitObservationEvidence {
            git_present: false,
            worktree_root: Some(root),
            worktree_git_dir: None,
            common_dir: None,
            head_kind: None,
            head_symref: None,
            head_oid: None,
            head_tree_oid: None,
            index_tree_oid: None,
            index_digest: None,
            refs_digest: None,
            index_locked: false,
            ref_locks: Vec::new(),
            operation_state: None,
            operation_markers: Vec::new(),
        },
        GitObservation::Repository(repository) => {
            let (head_kind, head_symref, head_oid) = match &repository.head {
                HeadObservation::Attached { symref, oid } => {
                    ("attached", Some(symref.clone()), Some(oid.to_string()))
                }
                HeadObservation::Detached { oid } => ("detached", None, Some(oid.to_string())),
                HeadObservation::Unborn { symref } => ("unborn", Some(symref.clone()), None),
                HeadObservation::MissingTarget { symref } => {
                    ("missing-target", Some(symref.clone()), None)
                }
            };
            GitObservationEvidence {
                git_present: true,
                worktree_root: repository.paths.worktree_root.clone(),
                worktree_git_dir: Some(repository.paths.worktree_git_dir.clone()),
                common_dir: Some(repository.paths.common_dir.clone()),
                head_kind: Some(head_kind),
                head_symref,
                head_oid,
                head_tree_oid: repository.head_tree_oid.map(|oid| oid.to_string()),
                index_tree_oid: repository.index.tree_oid.map(|oid| oid.to_string()),
                index_digest: Some(repository.index.canonical_digest.0.clone()),
                refs_digest: Some(repository.refs_digest.0.clone()),
                index_locked: repository.locks.index_lock.is_present(),
                ref_locks: repository.locks.ref_locks.clone(),
                operation_state: Some(repository.operation.repository_state.clone()),
                operation_markers: repository
                    .operation
                    .present_markers()
                    .into_iter()
                    .map(|marker| marker.as_str())
                    .collect(),
            }
        }
    }
}

fn warn_if_view_mismatch(root: &Path) {
    let atomic_view = match fs::read_to_string(root.join(".atomic/current_view")) {
        Ok(view) => view.trim().to_string(),
        Err(_) => return,
    };
    let git_branch = GitRepository::open(root).ok().and_then(|repository| {
        repository
            .head()
            .ok()
            .and_then(|head| head.shorthand().map(str::to_string))
    });
    if let Some(git_branch) = git_branch {
        if !atomic_view.is_empty() && git_branch != atomic_view {
            print_warning(&format!(
                "git is on '{git_branch}' but the Atomic view is '{atomic_view}'; run 'atomic status --no-reconcile' before taking action"
            ));
        }
    }
}

fn canonical_root(root: &Path) -> CliResult<PathBuf> {
    fs::canonicalize(root).map_err(|error| {
        git_error(format!(
            "cannot resolve repository root '{}': {error}",
            root.display()
        ))
    })
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn dispatcher_uses_absolute_binary_and_keeps_every_failure_advisory() {
        let script = dispatcher_script(Path::new("/opt/Atomic Tools/atomic")).unwrap();
        assert!(script.starts_with("#!/bin/sh\n# atomic:git-bridge-dispatcher:v1"));
        assert!(script.contains("'/opt/Atomic Tools/atomic' git bridge hook-post-checkout"));
        assert!(script.contains("\"$@\" || true"));
        assert!(script.contains("\"$0.d\"/*"));
        assert!(script.ends_with("exit 0\n"));
    }

    #[test]
    fn ownership_requires_the_exact_dispatcher_header() {
        assert!(is_owned_dispatcher(
            b"#!/bin/sh\n# atomic:git-bridge-dispatcher:v1\nexit 0\n"
        ));
        assert!(!is_owned_dispatcher(
            b"#!/bin/sh\necho custom\n# atomic:git-bridge-dispatcher:v1\n"
        ));
        assert!(!is_owned_dispatcher(b"\xff\xfeatomic"));
    }

    #[test]
    fn legacy_hook_is_owned_only_when_no_custom_content_surrounds_it() {
        let owned = b"#!/bin/sh\n\n# atomic:git:begin\natomic git import --incremental || true\n# atomic:git:end\n";
        let mixed = b"#!/bin/sh\necho custom\n# atomic:git:begin\natomic old\n# atomic:git:end\n";
        assert!(is_legacy_atomic_only(owned));
        assert!(!is_legacy_atomic_only(mixed));
    }

    #[test]
    fn event_journal_is_append_only_and_requests_are_unique() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".atomic")).unwrap();

        let first = persist_checkout_event(root.path(), OLD, NEW, "1").unwrap();
        let journal = root.path().join(EVENT_JOURNAL_RELATIVE);
        let prefix = fs::read(&journal).unwrap();
        let second = persist_checkout_event(root.path(), NEW, OLD, "0").unwrap();
        let complete = fs::read(&journal).unwrap();

        assert!(complete.starts_with(&prefix));
        assert_ne!(first.event_id, second.event_id);
        assert_ne!(first.request_path, second.request_path);
        assert!(first.request_path.exists());
        assert!(second.request_path.exists());
        let lines = String::from_utf8(complete).unwrap().lines().count();
        assert_eq!(lines, 2);
    }

    #[test]
    fn invalid_checkout_arguments_do_not_create_evidence() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(".atomic")).unwrap();
        assert!(persist_checkout_event(root.path(), "not-an-oid", NEW, "1").is_err());
        assert!(!root.path().join(EVENT_JOURNAL_RELATIVE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hook_is_never_replaced() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        let hook = root.path().join("post-checkout");
        fs::write(&target, b"custom target\n").unwrap();
        symlink(&target, &hook).unwrap();

        let result = install_or_refresh_dispatcher(&hook, "owned").unwrap();
        assert_eq!(result, DispatcherInstall::Unmanaged);
        assert_eq!(fs::read(&target).unwrap(), b"custom target\n");
        assert!(fs::symlink_metadata(&hook)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}

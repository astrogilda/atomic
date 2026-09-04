//! The shadow-commit pipeline — the single path that stages a shadow commit and
//! runs the pre-commit Validator before a git tree is produced.
//!
//! Per `SPEC-single-materializer-validator.md` (§5), exactly one code path may
//! stage the shadow working copy and hand a candidate to the Validator.
//! `atomic git push` and the turn-end hook both go through
//! [`stage_and_validate_tree`], so no other path independently
//! `git add -A`/`write_tree`s the tree. Any Validator rule failure aborts
//! atomically: the git index is restored from HEAD and nothing is committed.
//!
//! Validator rules enforced here (pre-commit):
//! - **V1** — no unresolved conflict markers (shares `record`'s detector).
//! - **V4** — no git-excluded provenance path (`.atomic/`, `.vault/`,
//!   `.atomicignore`) is ever staged.
//!
//! V2 (tree↔view coherence) and V3 (git↔state agreement) are added here in later
//! phases, ahead of `write_tree`, so every shadow commit passes the same gate.

use std::io::IsTerminal;
use std::path::Path;

use git2::Repository as GitRepository;

use atomic_repository::Repository;

use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

/// Result of coordinating an Atomic view switch with its local Git shadow.
#[derive(Debug)]
pub(crate) enum ShadowSwitchSync {
    /// The working copy is not inside a Git repository.
    SkippedNoGit,
    /// Git exists, but Atomic shadow sync has not been established.
    SkippedInactive,
    /// The target branch, HEAD, and index now describe the materialized view.
    Synchronized(ShadowSwitchReceipt),
}

impl ShadowSwitchSync {
    pub(crate) fn is_synchronized(&self) -> bool {
        matches!(self, Self::Synchronized(_))
    }

    /// Restore the Git evidence captured before synchronization. This is used
    /// when the final bridge checkpoint cannot be refreshed after Git itself
    /// was aligned successfully.
    pub(crate) fn rollback(self, repo_root: &Path) -> CliResult<()> {
        let Self::Synchronized(receipt) = self else {
            return Ok(());
        };
        let git_repo = GitRepository::discover(repo_root).map_err(|error| {
            git_error(format!(
                "cannot reopen Git repository to roll back shadow switch: {error}"
            ))
        })?;
        let failures = receipt.snapshot.restore(&git_repo, &receipt.target_ref);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(git_error(format!(
                "failed to restore original Git state: {}",
                failures.join("; ")
            )))
        }
    }
}

#[derive(Debug)]
pub(crate) struct ShadowSwitchReceipt {
    target_ref: String,
    snapshot: GitSwitchSnapshot,
}

#[derive(Clone, Debug)]
enum ReferenceSnapshot {
    Missing,
    Direct(git2::Oid),
    Symbolic(String),
}

#[derive(Clone, Debug)]
struct GitSwitchSnapshot {
    head: ReferenceSnapshot,
    target: ReferenceSnapshot,
    index: Option<Vec<u8>>,
}

impl GitSwitchSnapshot {
    fn capture(git_repo: &GitRepository, target_ref: &str) -> CliResult<Self> {
        let index_path = git_repo.path().join("index");
        let index = match std::fs::read(&index_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(git_error(format!(
                    "cannot snapshot Git index before shadow switch: {error}"
                )))
            }
        };
        Ok(Self {
            head: snapshot_reference(git_repo, "HEAD")?,
            target: snapshot_reference(git_repo, target_ref)?,
            index,
        })
    }

    fn restore(&self, git_repo: &GitRepository, target_ref: &str) -> Vec<String> {
        let mut failures = Vec::new();
        if let Err(error) = restore_head(git_repo, &self.head) {
            failures.push(format!("HEAD: {error}"));
        }
        if let Err(error) = restore_reference(git_repo, target_ref, &self.target) {
            failures.push(format!("{target_ref}: {error}"));
        }
        if let Err(error) = restore_index(git_repo, self.index.as_deref()) {
            failures.push(format!("index: {error}"));
        }
        failures
    }
}

/// Project the just-materialized Atomic view into its local Git mirror.
///
/// The working copy is staged exclusively through [`stage_and_validate_tree`].
/// If the target branch is stale, a local projection commit advances it from
/// its prior tip; a missing target branches from the current Git HEAD. HEAD and
/// the live index are then aligned to that exact tree without checking out or
/// otherwise touching the working copy.
///
/// Plain non-Git and non-shadow repositories are typed no-ops. Once shadow sync
/// is active, every failure is returned to the caller and the original Git
/// HEAD, target ref, and index are restored where possible.
pub(crate) fn sync_git_head_to_view(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<ShadowSwitchSync> {
    let git_repo = match GitRepository::discover(repo_root) {
        Ok(repo) => repo,
        Err(_) => return Ok(ShadowSwitchSync::SkippedNoGit),
    };
    if !shadow_sync_active(&git_repo) {
        return Ok(ShadowSwitchSync::SkippedInactive);
    }
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::Repository)?;
    if desired_view != view {
        return Err(git_error(format!(
            "cannot synchronize Git shadow for view '{view}': current Atomic view is '{desired_view}'"
        )));
    }

    // A coordinated switch must not silently skip its projection. Contention is
    // an actionable failure because leaving the old branch/index would publish
    // stale evidence for the newly materialized view.
    let _shadow_lock = repo
        .try_lock_shadow_commit()
        .map_err(CliError::Repository)?
        .ok_or_else(|| {
            git_error(
                "another shadow materialize is in flight; retry the view switch after it completes",
            )
        })?;

    let target_ref = format!("refs/heads/{view}");
    let snapshot = GitSwitchSnapshot::capture(&git_repo, &target_ref)?;
    let sync_result = (|| -> CliResult<()> {
        // Never bypass V1 for a switch: a conflicted materialization cannot be
        // checkpointed as clean Git evidence.
        let tree_oid = stage_and_validate_tree(repo, &git_repo, repo_root, view, false)?;
        let state = repo
            .get_view_info(view)
            .map_err(CliError::Repository)?
            .state_base32();
        let commit =
            find_or_create_switch_projection(&git_repo, &target_ref, view, &state, tree_oid)?;
        update_target_ref(&git_repo, &target_ref, commit)?;
        git_repo.set_head(&target_ref).map_err(|error| {
            git_error(format!(
                "cannot point Git HEAD at shadow branch '{view}': {error}"
            ))
        })?;
        align_index_to_tree(&git_repo, tree_oid)?;
        Ok(())
    })();

    match sync_result {
        Ok(()) => Ok(ShadowSwitchSync::Synchronized(ShadowSwitchReceipt {
            target_ref,
            snapshot,
        })),
        Err(error) => {
            let rollback_failures = snapshot.restore(&git_repo, &target_ref);
            if rollback_failures.is_empty() {
                Err(error)
            } else {
                Err(git_error(format!(
                    "{error}; additionally failed to restore original Git state: {}",
                    rollback_failures.join("; ")
                )))
            }
        }
    }
}

fn find_or_create_switch_projection(
    git_repo: &GitRepository,
    target_ref: &str,
    view: &str,
    state: &str,
    tree_oid: git2::Oid,
) -> CliResult<git2::Oid> {
    let parent_oid = match git_repo.find_reference(target_ref) {
        Ok(reference) => {
            let commit = reference.peel_to_commit().map_err(|error| {
                git_error(format!("cannot read target shadow branch '{view}': {error}"))
            })?;
            if commit.tree_id() == tree_oid {
                return Ok(commit.id());
            }
            commit.id()
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => git_repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| {
                git_error(format!(
                    "cannot create shadow branch '{view}' without a current Git HEAD commit: {error}"
                ))
            })?
            .id(),
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target shadow branch '{view}': {error}"
            )))
        }
    };
    let parent = git_repo.find_commit(parent_oid).map_err(|error| {
        git_error(format!(
            "cannot read parent for shadow projection on '{view}': {error}"
        ))
    })?;
    let tree = git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "cannot read validated tree for shadow projection on '{view}': {error}"
        ))
    })?;
    let signature = git_repo.signature().map_err(|error| {
        git_error(format!(
            "cannot determine Git signature for shadow projection: {error}"
        ))
    })?;
    let message =
        format!("Atomic shadow switch projection\n\nAtomic-View: {view}\nAtomic-State: {state}\n");
    let commit = git_repo
        .commit(None, &signature, &signature, &message, &tree, &[&parent])
        .map_err(|error| {
            git_error(format!(
                "cannot create local shadow projection for view '{view}': {error}"
            ))
        })?;
    Ok(commit)
}

fn update_target_ref(
    git_repo: &GitRepository,
    target_ref: &str,
    commit: git2::Oid,
) -> CliResult<()> {
    match git_repo.find_reference(target_ref) {
        Ok(mut reference) => {
            if reference.target() != Some(commit) {
                reference
                    .set_target(commit, "atomic shadow view switch")
                    .map_err(|error| {
                        git_error(format!(
                            "cannot advance target shadow branch '{target_ref}': {error}"
                        ))
                    })?;
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            git_repo
                .reference(target_ref, commit, false, "atomic shadow view switch")
                .map_err(|error| {
                    git_error(format!(
                        "cannot create target shadow branch '{target_ref}': {error}"
                    ))
                })?;
        }
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target shadow branch '{target_ref}': {error}"
            )))
        }
    }
    Ok(())
}

fn align_index_to_tree(git_repo: &GitRepository, tree_oid: git2::Oid) -> CliResult<()> {
    let tree = git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "cannot read shadow projection tree while aligning Git index: {error}"
        ))
    })?;
    let mut index = git_repo.index().map_err(|error| {
        git_error(format!(
            "cannot open Git index while aligning shadow view: {error}"
        ))
    })?;
    index
        .read_tree(&tree)
        .and_then(|_| index.write())
        .map_err(|error| {
            git_error(format!(
                "cannot align Git index to the shadow projection tree: {error}"
            ))
        })
}

fn snapshot_reference(git_repo: &GitRepository, name: &str) -> CliResult<ReferenceSnapshot> {
    match git_repo.find_reference(name) {
        Ok(reference) => {
            if let Some(target) = reference.target() {
                Ok(ReferenceSnapshot::Direct(target))
            } else if let Some(target) = reference.symbolic_target() {
                Ok(ReferenceSnapshot::Symbolic(target.to_string()))
            } else {
                Err(git_error(format!(
                    "cannot snapshot Git reference '{name}': it has no target"
                )))
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(ReferenceSnapshot::Missing),
        Err(error) => Err(git_error(format!(
            "cannot snapshot Git reference '{name}': {error}"
        ))),
    }
}

fn restore_head(git_repo: &GitRepository, snapshot: &ReferenceSnapshot) -> Result<(), git2::Error> {
    match snapshot {
        ReferenceSnapshot::Missing => match git_repo.find_reference("HEAD") {
            Ok(mut reference) => reference.delete(),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        ReferenceSnapshot::Direct(target) => git_repo.set_head_detached(*target),
        ReferenceSnapshot::Symbolic(target) => git_repo.set_head(target),
    }
}

fn restore_reference(
    git_repo: &GitRepository,
    name: &str,
    snapshot: &ReferenceSnapshot,
) -> Result<(), git2::Error> {
    match snapshot {
        ReferenceSnapshot::Missing => match git_repo.find_reference(name) {
            Ok(mut reference) => reference.delete(),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        ReferenceSnapshot::Direct(target) => git_repo
            .reference(name, *target, true, "restore failed Atomic shadow switch")
            .map(|_| ()),
        ReferenceSnapshot::Symbolic(target) => git_repo
            .reference_symbolic(name, target, true, "restore failed Atomic shadow switch")
            .map(|_| ()),
    }
}

fn restore_index(git_repo: &GitRepository, bytes: Option<&[u8]>) -> std::io::Result<()> {
    let index_path = git_repo.path().join("index");
    match bytes {
        Some(bytes) => std::fs::write(index_path, bytes),
        None => match std::fs::remove_file(index_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}

/// Whether git shadow sync is established for this repo (the `.git/info/exclude`
/// carries Atomic's shadow patterns, written by import/push). Used to gate the
/// view-switch git-follow so we never touch HEAD in a plain (non-shadow) repo.
fn shadow_sync_active(git_repo: &GitRepository) -> bool {
    let exclude = git_repo.path().join("info").join("exclude");
    std::fs::read_to_string(exclude)
        .map(|c| c.lines().any(|l| l.trim() == "/.atomic/"))
        .unwrap_or(false)
}

/// Acquire the repo-scoped shadow-commit lock, or return `None` (a no-op skip)
/// if a shadow materialize/commit is already in flight (SPEC §4.3 / Principle 5).
///
/// Non-blocking: rather than queueing (which would hang a turn-end hook), the
/// contended case is a logged no-op — the in-flight operation owns this commit.
/// The returned guard must be held for the whole stage → validate → commit
/// sequence; dropping it releases the lock. Acquire it **outermost**, before any
/// staging or DB write.
pub(crate) fn acquire_shadow_lock(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<Option<std::fs::File>> {
    match repo
        .try_lock_shadow_commit()
        .map_err(CliError::Repository)?
    {
        Some(guard) => Ok(Some(guard)),
        None => {
            if std::io::stderr().is_terminal() {
                print_info("Another shadow materialize is in flight; skipping this push.");
            } else {
                append_shadow_log(
                    repo_root,
                    "shadow-lock:contended",
                    view,
                    "another shadow materialize in flight",
                );
            }
            Ok(None)
        }
    }
}

/// Stage the current working copy for a shadow commit, run the pre-commit
/// Validator, and return the candidate git tree OID.
///
/// This is the sole shadow-commit staging path (SPEC §5.2). On any Validator
/// failure it aborts atomically — the index is restored from HEAD so git is left
/// byte-identical — and returns an error naming the failing rule.
pub(crate) fn stage_and_validate_tree(
    repo: &Repository,
    git_repo: &GitRepository,
    repo_root: &Path,
    view: &str,
    allow_conflict_markers: bool,
) -> CliResult<git2::Oid> {
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;

    // ── Rule V1 — no unresolved conflict markers ────────────────────────────
    // Shares `atomic record`'s detector so the two paths cannot disagree.
    if !allow_conflict_markers {
        if let Some((path, line)) = repo
            .first_working_copy_conflict_marker(working_copy)
            .map_err(CliError::Repository)?
        {
            if !std::io::stderr().is_terminal() {
                append_shadow_validate_log(
                    repo_root,
                    "V1",
                    view,
                    &format!("file={} line={}", path, line),
                );
            }
            print_warning(&format!(
                "Refusing to commit '{}': unresolved conflict marker at line {}.",
                path, line
            ));
            return Err(CliError::GitError {
                message: format!(
                    "'{}' still contains conflict markers at line {} — resolve the \
                     conflict (remove the >>>>>>> / ======= / <<<<<<< lines), or pass \
                     --allow-conflict-markers to override. No commit was created.",
                    path, line
                ),
            });
        }
    }

    // Prevention: make sure git is configured to exclude Atomic's shadow /
    // provenance paths before staging, so `git add -A` never picks them up.
    // Best-effort (an unwritable .git/info is caught by the V4 guard below).
    let _ = super::import::ensure_git_shadow_excludes(git_repo.path());

    // Stage everything: git add -A (add_all + update_all handles new files and
    // deletions).
    let mut index = git_repo.index().map_err(|e| CliError::GitError {
        message: format!("Failed to open git index: {}", e),
    })?;
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(|e| CliError::GitError {
            message: format!("Failed to stage files: {}", e),
        })?;
    index
        .update_all(["*"].iter(), None)
        .map_err(|e| CliError::GitError {
            message: format!("Failed to update index: {}", e),
        })?;
    index.write().map_err(|e| CliError::GitError {
        message: format!("Failed to write index: {}", e),
    })?;

    // ── Rule V4 — no provenance / excluded path may be staged ───────────────
    if let Some(bad) = first_forbidden_shadow_path(&index) {
        restore_index_from_head(git_repo);
        if !std::io::stderr().is_terminal() {
            append_shadow_validate_log(repo_root, "V4", view, &format!("path={}", bad));
        }
        print_warning(&format!(
            "Refusing to shadow-commit: provenance/excluded path '{}' was staged.",
            bad
        ));
        return Err(CliError::GitError {
            message: format!(
                "'{}' is a git-excluded Atomic shadow path (.atomic/, .vault/, \
                 .atomicignore) and must never be committed to git. Aborting; no \
                 commit was created and the index was restored. Ensure \
                 `.git/info/exclude` carries the Atomic shadow patterns.",
                bad
            ),
        });
    }

    let tree_oid = index.write_tree().map_err(|e| CliError::GitError {
        message: format!("Failed to write tree: {}", e),
    })?;

    // ── Rule V2 — tree ↔ view coherence (SPEC §6.2) ─────────────────────────
    // The staged tree must correspond to what the current view materializes.
    // Cost-safe / incremental: only paths that differ between the candidate
    // tree and git HEAD are checked, each against the view's recorded content.
    if let Some((path, reason)) = first_incoherent_path(repo, git_repo, tree_oid, view)? {
        restore_index_from_head(git_repo);
        if !std::io::stderr().is_terminal() {
            append_shadow_validate_log(
                repo_root,
                "V2",
                view,
                &format!("path={} reason={}", path, reason),
            );
        }
        print_warning(&format!(
            "Refusing to shadow-commit: '{}' {} (SPEC V2).",
            path, reason
        ));
        return Err(CliError::GitError {
            message: format!(
                "'{}' {} — the working copy diverges from the current view '{}'. Record \
                 your changes (or reconcile the view) so the shadow tree matches the \
                 recorded state. No commit was created.",
                path, reason, view
            ),
        });
    }

    Ok(tree_oid)
}

/// Return the first changed path whose staged content does not correspond to the
/// current view's recorded content, as `(path, reason)`, or `None` if the
/// candidate tree is coherent with the view (Rule V2, SPEC §6.2).
///
/// Only paths that differ between the candidate tree and git HEAD are examined
/// (the incremental form), so the check costs one `get_file_content_on_view` per
/// changed path rather than a full-view materialize. Provenance / excluded paths
/// are skipped — Rule V4 owns them.
fn first_incoherent_path(
    repo: &Repository,
    git_repo: &GitRepository,
    candidate_tree_oid: git2::Oid,
    view: &str,
) -> CliResult<Option<(String, String)>> {
    let candidate_tree =
        git_repo
            .find_tree(candidate_tree_oid)
            .map_err(|e| CliError::GitError {
                message: format!("Failed to load candidate tree: {}", e),
            })?;
    let head_tree = git_repo
        .head()
        .ok()
        .and_then(|h| h.peel_to_commit().ok())
        .and_then(|c| c.tree().ok());

    let diff = git_repo
        .diff_tree_to_tree(head_tree.as_ref(), Some(&candidate_tree), None)
        .map_err(|e| CliError::GitError {
            message: format!("Failed to diff candidate tree: {}", e),
        })?;

    for delta in diff.deltas() {
        let (path, in_candidate) = match delta.status() {
            git2::Delta::Deleted => match delta.old_file().path().and_then(|p| p.to_str()) {
                Some(p) => (p.to_string(), false),
                None => continue,
            },
            _ => match delta.new_file().path().and_then(|p| p.to_str()) {
                Some(p) => (p.to_string(), true),
                None => continue,
            },
        };

        // Rule V4 owns provenance / git-excluded paths; V2 ignores them.
        if is_forbidden_shadow_path(&path) {
            continue;
        }

        let view_content = repo
            .get_file_content_on_view(&path, view)
            .map_err(CliError::Repository)?;

        if in_candidate {
            // The staged blob must equal what the view materializes for this path.
            let staged = git_repo.find_blob(delta.new_file().id()).ok();
            match (
                staged.as_ref().map(|b| b.content()),
                view_content.as_deref(),
            ) {
                (Some(s), Some(v)) if s == v => {}
                (Some(_), Some(_)) => {
                    return Ok(Some((
                        path,
                        "staged content differs from the view's recorded content".to_string(),
                    )));
                }
                (Some(_), None) => {
                    return Ok(Some((
                        path,
                        "is not recorded by the view (record it first)".to_string(),
                    )));
                }
                // Non-blob entries (submodules/symlinks) carry no textual
                // content to reconcile; leave them to git's own handling.
                (None, _) => {}
            }
        } else if view_content.is_some() {
            // The path was dropped from the tree, but the view still records it:
            // the candidate omits a change the view accounts for.
            return Ok(Some((
                path,
                "is still recorded by the view but missing from the tree".to_string(),
            )));
        }
    }

    Ok(None)
}

/// Append a `shadow-validate:<rule>` entry to `.atomic/hook-errors.log` (SPEC
/// §6.5) so a non-interactive shadow push that a Validator rule aborts leaves a
/// durable, greppable trail instead of failing silently.
pub(crate) fn append_shadow_validate_log(repo_root: &Path, rule: &str, view: &str, detail: &str) {
    append_shadow_log(
        repo_root,
        &format!("shadow-validate:{}", rule),
        view,
        detail,
    );
}

/// Append one tagged `.atomic/hook-errors.log` line. Best-effort: log I/O errors
/// are ignored (the operation already surfaces its own outcome).
fn append_shadow_log(repo_root: &Path, tag: &str, view: &str, detail: &str) {
    use std::io::Write;
    let log_path = repo_root.join(".atomic").join("hook-errors.log");
    let entry = format!(
        "{} {} view={} {}\n",
        chrono::Utc::now().to_rfc3339(),
        tag,
        view,
        detail
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| f.write_all(entry.as_bytes()));
}

/// Return the first staged index path that is a git-excluded shadow / provenance
/// path (`.atomic/`, `.vault/`, or `.atomicignore`), or `None` if the candidate
/// is clean. Validator Rule V4 (SPEC §6.4): these paths must never enter a git
/// commit — `.vault` (intents/memories/attestations) and `.atomic` (the change
/// graph) are git-excluded and unbacked; committing or reconciling them risks
/// the provenance layer.
fn first_forbidden_shadow_path(index: &git2::Index) -> Option<String> {
    index.iter().find_map(|entry| {
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        is_forbidden_shadow_path(&path).then_some(path)
    })
}

/// Whether `path` (a repo-relative git path) is a git-excluded Atomic shadow /
/// provenance path that Rule V4 forbids from any shadow commit.
fn is_forbidden_shadow_path(path: &str) -> bool {
    path == ".atomicignore" || path.starts_with(".atomic/") || path.starts_with(".vault/")
}

/// Discard a candidate staging by restoring the git index from HEAD's tree,
/// leaving git byte-identical to its pre-operation state (the working copy is
/// never touched). Best-effort.
fn restore_index_from_head(git_repo: &GitRepository) {
    if let Ok(tree) = git_repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .and_then(|c| c.tree())
    {
        if let Ok(mut index) = git_repo.index() {
            let _ = index.read_tree(&tree);
            let _ = index.write();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_forbidden_shadow_path as forbidden;

    #[test]
    fn forbids_provenance_and_excluded_paths() {
        assert!(forbidden(".atomicignore"));
        assert!(forbidden(".atomic/pristine.redb"));
        assert!(forbidden(".vault/intents/foo.md"));
    }

    #[test]
    fn allows_ordinary_source_paths() {
        assert!(!forbidden("src/main.rs"));
        assert!(!forbidden("README.md"));
        // A file that merely *contains* the substring is not forbidden.
        assert!(!forbidden("docs/.atomicignore.md"));
        assert!(!forbidden("my.vault/keep.txt"));
    }
}

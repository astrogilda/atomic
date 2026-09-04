//! Minimal experimental bridge between a clean Git checkout and an Atomic view.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::pristine::{GraphTxnT, ViewTxnT};
use atomic_core::types::WorkingCopyId;
use atomic_repository::{
    graph_visibility_closure, InsertOptions, Repository, RepositoryError, StatusOptions,
};
use clap::{Parser, Subcommand};
use git2::{
    ObjectType, Oid, Repository as GitRepository, Status, StatusOptions as GitStatusOptions,
};

use super::checkpoint::{self, BridgeCheckpoint, VerifiedCheckpointInput};
use super::observation::{
    observe_git, observe_head, BridgeCheckpointObservation, GitObservation, HeadObservation,
    ObservationError,
};
use super::{hooks, Import};
use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::print_success;

/// Reconcile and verify a clean, attached Git checkout against Atomic.
#[derive(Parser, Debug, Default)]
#[command(name = "bridge")]
pub struct Bridge {
    #[command(subcommand)]
    pub command: BridgeCommand,
}

#[derive(Subcommand, Debug, Default)]
pub enum BridgeCommand {
    /// Incrementally import Git HEAD, verify it, and record bridge metadata.
    #[default]
    Reconcile,
    /// Read-only comparison of Git HEAD and the current Atomic view.
    Verify,
    /// Switch Git and Atomic together to an existing view.
    Switch { view: String },
    /// Enable the advisory Git checkout event bridge.
    Enable,
    /// Internal callback used only by the Atomic-owned post-checkout dispatcher.
    #[command(hide = true)]
    HookPostCheckout {
        old_head: String,
        new_head: String,
        checkout_flag: String,
    },
    /// Internal worker for one immutable deferred-observation request.
    #[command(hide = true)]
    ObserveDeferred {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        request: PathBuf,
    },
}

impl Command for Bridge {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            BridgeCommand::Reconcile => reconcile(),
            BridgeCommand::Verify => {
                verify()?;
                print_success("Git HEAD matches the current Atomic view");
                Ok(())
            }
            BridgeCommand::Switch { view } => switch(view),
            BridgeCommand::Enable => {
                let root = find_repository_root()?;
                hooks::enable_bridge(&root)
            }
            BridgeCommand::HookPostCheckout {
                old_head,
                new_head,
                checkout_flag,
            } => {
                let root = find_repository_root()?;
                hooks::record_post_checkout(&root, old_head, new_head, checkout_flag)
            }
            BridgeCommand::ObserveDeferred { root, request } => {
                hooks::run_deferred_observation(root, request)
            }
        }
    }
}

#[derive(Debug)]
struct BridgeSnapshot {
    view: String,
    atomic_state: String,
    git_head: String,
    git_tree: String,
}

#[derive(Debug, Eq, PartialEq)]
enum ReconcileDirection {
    Neither,
    GitToAtomic,
    AtomicToGit,
    Diverged,
}

fn classify_direction(
    checkpoint_view: &str,
    checkpoint_git_head: &str,
    checkpoint_atomic_state: &str,
    current_git_branch: &str,
    current_git_head: &str,
    current_atomic_state: &str,
) -> ReconcileDirection {
    let git_changed =
        checkpoint_view != current_git_branch || checkpoint_git_head != current_git_head;
    match (git_changed, checkpoint_atomic_state != current_atomic_state) {
        (false, false) => ReconcileDirection::Neither,
        (true, false) => ReconcileDirection::GitToAtomic,
        (false, true) => ReconcileDirection::AtomicToGit,
        (true, true) => ReconcileDirection::Diverged,
    }
}

fn reconcile() -> CliResult<()> {
    let root = find_repository_root()?;
    let repo = Repository::open(&root).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    let git = open_git(&root)?;
    let current = current_heads(&repo, working_copy, &git)?;
    let current_git_branch = current_attached_git_branch(&git)?;
    let checkpoint = read_workspace_metadata(&root)?;

    let direction = checkpoint
        .as_ref()
        .map(|checkpoint| {
            classify_direction(
                &checkpoint.view,
                &checkpoint.git_head,
                &checkpoint.atomic_state,
                &current_git_branch,
                &current.git_head,
                &current.atomic_state,
            )
        })
        .unwrap_or(ReconcileDirection::GitToAtomic);

    match direction {
        ReconcileDirection::Neither => {
            drop(git);
            drop(repo);
            verify_at(&root)?;
            print_success("Git HEAD already matches the current Atomic view");
            Ok(())
        }
        ReconcileDirection::GitToAtomic => {
            // `Import::run` reopens the repository. Release these handles first
            // so redb does not reject a second open in the same process.
            drop(git);
            drop(repo);
            import_git_to_atomic(&root)
        }
        ReconcileDirection::AtomicToGit => {
            project_atomic_to_git(&root, &repo, working_copy, &git, &current)?;
            drop(git);
            drop(repo);
            let snapshot = verify_at(&root)?;
            write_workspace_metadata(&root, &snapshot)?;
            print_success("Projected the current Atomic view to Git HEAD");
            Ok(())
        }
        ReconcileDirection::Diverged => Err(git_error(
            "Git HEAD and Atomic state both changed since the bridge checkpoint; reconcile the divergence explicitly",
        )),
    }
}

fn switch(target: &str) -> CliResult<()> {
    let root = find_repository_root()?;
    let checkpoint = read_workspace_metadata(&root)?
        .ok_or_else(|| git_error("bridge switch requires an existing checkpoint; run 'atomic git bridge reconcile' first"))?;
    let target_state = {
        let repo = Repository::open(&root).map_err(CliError::from)?;
        let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
        let git = open_git(&root)?;
        let current = current_heads(&repo, working_copy, &git)?;
        if current_attached_git_branch(&git)? != current.view
            || !checkpoint_matches_snapshot(&checkpoint, &current)
        {
            return Err(git_error(
                "current Git and Atomic state does not match the bridge checkpoint; reconcile before switching",
            ));
        }

        let target_state = repo
            .get_view_info(target)
            .map_err(CliError::from)?
            .state
            .to_string();
        let current_paths = git_head_paths(&git)?;
        let target_paths = git_branch_paths(&git, target)?;
        plan_switch_collisions(&root, &current_paths, &target_paths).map_err(git_error)?;
        target_state
    };

    // After collision-specific checks, require the full clean/equality invariant.
    verify_at(&root)?;
    let mut repo = Repository::open(&root).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;

    if std::env::var("ATOMIC_TEST_BRIDGE_FAIL_BEFORE_MATERIALIZE").as_deref() == Ok("1") {
        return Err(git_error(
            "bridge switch stopped before Atomic materialization by ATOMIC_TEST_BRIDGE_FAIL_BEFORE_MATERIALIZE",
        ));
    }

    // The current TREE projection is not view-scoped enough to build a reliable
    // manifest for an inactive view. For this foreground MVP, materialize the
    // target exactly once, verify Atomic considers it clean, then project that
    // result into Git. The full RFC replaces this with a view-scoped manifest
    // so Git can be prepared before filesystem mutation.
    repo.switch_view(working_copy, target)
        .map_err(CliError::from)?;
    let atomic_status = repo
        .status(working_copy, StatusOptions::default())
        .map_err(CliError::from)?;
    if !atomic_status.is_clean() {
        return Err(git_error(
            "Atomic materialization did not produce a clean target view",
        ));
    }
    let target_files = read_filesystem(&root)?;

    let git = open_git(&root)?;
    require_clean_git_index(&git)?;
    let target_tree_oid = write_git_tree(&git, &target_files)?;
    let target_commit_oid =
        find_or_create_target_commit(&git, target, target_tree_oid, &target_state)?;
    let target_ref = format!("refs/heads/{target}");
    update_target_branch(&git, &target_ref, target_commit_oid)?;
    git.set_head(&target_ref)
        .map_err(|error| git_error(format!("cannot attach Git HEAD to '{target}': {error}")))?;
    let target_tree = git
        .find_tree(target_tree_oid)
        .map_err(|error| git_error(format!("cannot read target Git tree: {error}")))?;
    let mut index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    index
        .read_tree(&target_tree)
        .and_then(|_| index.write())
        .map_err(|error| git_error(format!("cannot reset Git index to target tree: {error}")))?;
    drop(index);
    drop(target_tree);
    drop(git);
    drop(repo);

    let snapshot = verify_at(&root)?;
    write_workspace_metadata(&root, &snapshot)?;
    let message = format!("Switched Git and Atomic to view '{target}'");
    print_success(&message);
    Ok(())
}

fn git_head_paths(git: &GitRepository) -> CliResult<BTreeSet<String>> {
    let tree = git
        .head()
        .and_then(|head| head.peel_to_commit())
        .and_then(|commit| commit.tree())
        .map_err(|error| git_error(format!("cannot read current Git HEAD tree: {error}")))?;
    Ok(read_git_tree(git, &tree)?.into_keys().collect())
}

fn git_branch_paths(git: &GitRepository, branch: &str) -> CliResult<BTreeSet<String>> {
    let reference_name = format!("refs/heads/{branch}");
    let tree = git
        .find_reference(&reference_name)
        .and_then(|reference| reference.peel_to_commit())
        .and_then(|commit| commit.tree())
        .map_err(|error| {
            if error.code() == git2::ErrorCode::NotFound {
                git_error(format!(
                    "target Git branch '{branch}' does not exist; bridge switch requires an existing target branch"
                ))
            } else {
                git_error(format!("cannot read target Git branch '{branch}': {error}"))
            }
        })?;
    Ok(read_git_tree(git, &tree)?.into_keys().collect())
}

fn plan_switch_collisions(
    root: &Path,
    current_paths: &BTreeSet<String>,
    target_paths: &BTreeSet<String>,
) -> Result<(), String> {
    for path in target_paths.difference(current_paths) {
        let relative = Path::new(path);
        let mut parent = relative.parent();
        while let Some(component) = parent {
            if component.as_os_str().is_empty() {
                break;
            }
            match fs::symlink_metadata(root.join(component)) {
                Ok(metadata) if !metadata.file_type().is_dir() => {
                    return Err(format!(
                        "cannot switch: target path '{path}' has non-directory parent '{}'",
                        component.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "cannot inspect target path parent '{}': {error}",
                        component.display()
                    ));
                }
            }
            parent = component.parent();
        }

        match fs::symlink_metadata(root.join(relative)) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                return Err(format!(
                    "cannot switch: target file path '{path}' is occupied by a directory"
                ));
            }
            Ok(_) => {
                return Err(format!(
                    "cannot switch: target tracked path '{path}' is occupied in the working tree"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!("cannot inspect target path '{path}': {error}"));
            }
        }
    }
    Ok(())
}

fn checkpoint_matches_snapshot(checkpoint: &BridgeCheckpoint, snapshot: &BridgeSnapshot) -> bool {
    checkpoint.view == snapshot.view
        && checkpoint.atomic_state == snapshot.atomic_state
        && checkpoint.git_head == snapshot.git_head
        && checkpoint.git_tree == snapshot.git_tree
}

fn find_or_create_target_commit(
    git: &GitRepository,
    target: &str,
    target_tree_oid: Oid,
    atomic_state: &str,
) -> CliResult<Oid> {
    let target_ref = format!("refs/heads/{target}");
    let parent_oid = match git.find_reference(&target_ref) {
        Ok(reference) => {
            let commit = reference.peel_to_commit().map_err(|error| {
                git_error(format!("cannot read target branch '{target}': {error}"))
            })?;
            if commit.tree_id() == target_tree_oid {
                return Ok(commit.id());
            }
            // Append to the target branch so updating it is a fast-forward;
            // never parent from another branch and force-move target history.
            commit.id()
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => git
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| git_error(format!("cannot read current Git HEAD commit: {error}")))?
            .id(),
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target branch '{target}': {error}"
            )))
        }
    };
    let parent = git
        .find_commit(parent_oid)
        .map_err(|error| git_error(format!("cannot read target parent commit: {error}")))?;
    let tree = git
        .find_tree(target_tree_oid)
        .map_err(|error| git_error(format!("cannot read target Git tree: {error}")))?;
    let signature = git
        .signature()
        .map_err(|error| git_error(format!("cannot determine Git signature: {error}")))?;
    let message = format!(
        "Atomic bridge switch projection\n\nAtomic-View: {target}\nAtomic-State: {atomic_state}\n"
    );
    git.commit(None, &signature, &signature, &message, &tree, &[&parent])
        .map_err(|error| {
            git_error(format!(
                "cannot create target compatibility commit: {error}"
            ))
        })
}

fn update_target_branch(git: &GitRepository, target_ref: &str, commit: Oid) -> CliResult<()> {
    match git.find_reference(target_ref) {
        Ok(mut reference) => {
            if reference.target() != Some(commit) {
                reference
                    .set_target(commit, "atomic bridge switch")
                    .map_err(|error| git_error(format!("cannot update '{target_ref}': {error}")))?;
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            git.reference(target_ref, commit, false, "atomic bridge switch")
                .map_err(|error| git_error(format!("cannot create '{target_ref}': {error}")))?;
        }
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target branch '{target_ref}': {error}"
            )))
        }
    }
    Ok(())
}

fn import_git_to_atomic(root: &Path) -> CliResult<()> {
    let git = open_git(root)?;
    let view = current_attached_git_branch(&git)?;
    require_clean_git_worktree(&git)?;
    let git_head = git
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|oid| oid.to_string())
        .ok_or_else(|| git_error("Git HEAD does not point directly to a commit"))?;
    drop(git);

    // `git checkout -b topic` changes only the symbolic branch while keeping
    // the bound commit/tree. Reuse the existing validated Atomic closure by
    // creating a self-contained shared view instead of replaying Git history.
    if let Some(checkpoint) = read_workspace_metadata(root)? {
        let mut repo = Repository::open(root).map_err(CliError::from)?;
        let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
        let view_exists = repo.view_exists(&view).map_err(CliError::from)?;
        if git_head == checkpoint.git_head && !view_exists {
            // Validate and resolve the complete dependency closure before the
            // first mutation. A legacy source with missing dependency metadata
            // must not leave a partially created adoption view behind.
            let closure = {
                let txn = repo.pristine().read_txn().map_err(|error| {
                    CliError::from(RepositoryError::Database(error.to_string()))
                })?;
                let source = txn
                    .get_view(&checkpoint.view)
                    .map_err(|error| CliError::from(RepositoryError::Database(error.to_string())))?
                    .ok_or_else(|| {
                        CliError::from(RepositoryError::ViewNotFound {
                            name: checkpoint.view.clone(),
                        })
                    })?;
                let visibility = graph_visibility_closure(&txn, &source).map_err(CliError::from)?;
                let mut hashes = Vec::with_capacity(visibility.len());
                for change_id in visibility.iter_dependency_first().copied() {
                    let hash = txn
                        .get_external(change_id)
                        .map_err(|error| {
                            CliError::from(RepositoryError::Database(error.to_string()))
                        })?
                        .ok_or_else(|| {
                            CliError::from(RepositoryError::Database(format!(
                                "visible change {} has no external hash",
                                change_id.get()
                            )))
                        })?;
                    hashes.push(hash);
                }
                hashes
            };
            repo.create_shared_view(&view).map_err(CliError::from)?;
            for hash in closure {
                repo.insert_change(&hash, InsertOptions::with_dependencies().view(&view))
                    .map_err(CliError::from)?;
            }
            repo.align_to_view(working_copy, &view)
                .map_err(CliError::from)?;
            repo.reindex_working_copy(working_copy)
                .map_err(CliError::from)?;
            drop(repo);
            let snapshot = verify_at(root)?;
            write_workspace_metadata(root, &snapshot)?;
            print_success("Adopted new Git branch from the existing Atomic closure");
            return Ok(());
        }
    }

    Import {
        incremental: true,
        branch: Some(view.clone()),
        no_vault: true,
        with_crdt: false,
        skip_checkpoint_refresh: true,
        ..Import::default()
    }
    .run()?;

    // Incremental import deliberately preserves the old Atomic working-copy
    // pointer when Git switched to another branch. Adopt the imported view by
    // aligning deferred TREE metadata and rebuilding FILE_INDEX only; neither
    // operation writes source files.
    let mut repo = Repository::open(root).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    repo.align_to_view(working_copy, &view)
        .map_err(CliError::from)?;
    repo.reindex_working_copy(working_copy)
        .map_err(CliError::from)?;
    drop(repo);

    let snapshot = verify_at(root)?;
    write_workspace_metadata(root, &snapshot)?;
    print_success("Reconciled Git HEAD with the current Atomic view");
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckpointRefresh {
    Refreshed,
    SkippedNoGit,
    SkippedUnsupportedHead,
    SkippedViewMismatch,
}

/// Refresh the local v2 checkpoint only when the existing bridge verifier can
/// prove that Git and the persisted Atomic working-copy view are fully aligned.
///
/// No-Git repositories and intentional branch/view mismatches are no-ops. The
/// latter is required by bridge incremental raw-switch adoption: `Import::run`
/// preserves the old Atomic pointer until `import_git_to_atomic` aligns it.
pub(crate) fn refresh_checkpoint_if_aligned(root: &Path) -> CliResult<CheckpointRefresh> {
    let observation = observe_git(root).map_err(observation_error)?;
    let GitObservation::Repository(git) = observation else {
        return Ok(CheckpointRefresh::SkippedNoGit);
    };
    let HeadObservation::Attached { symref, .. } = &git.head else {
        return Ok(CheckpointRefresh::SkippedUnsupportedHead);
    };
    let Some(branch) = symref.strip_prefix("refs/heads/") else {
        return Ok(CheckpointRefresh::SkippedUnsupportedHead);
    };

    let current_view = {
        let repo = Repository::open(root).map_err(CliError::from)?;
        let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
        repo.desired_view_name(working_copy)
            .map_err(CliError::from)?
    };
    if branch != current_view {
        return Ok(CheckpointRefresh::SkippedViewMismatch);
    }

    let snapshot = verify_at(root)?;
    write_workspace_metadata(root, &snapshot)?;
    Ok(CheckpointRefresh::Refreshed)
}

fn verify() -> CliResult<BridgeSnapshot> {
    let root = find_repository_root()?;
    verify_at(&root)
}

fn verify_at(root: &Path) -> CliResult<BridgeSnapshot> {
    let repo = Repository::open(root).map_err(CliError::from)?;
    let working_copy = repo.require_working_copy_id().map_err(CliError::from)?;
    let git = open_git(root)?;
    let (branch, head, tree) = require_matching_clean_workspaces(&repo, working_copy, &git, true)?;
    let git_files = read_git_tree(&git, &tree)?;
    let worktree_files = read_filesystem(root)?;
    compare_file_sets(&git_files, &worktree_files).map_err(|error| {
        git_error(format!(
            "Git HEAD and the Atomic-clean working tree differ: {error}"
        ))
    })?;
    let atomic_state = repo
        .get_view_info(&branch)
        .map_err(CliError::from)?
        .state
        .to_string();

    Ok(BridgeSnapshot {
        view: branch,
        atomic_state,
        git_head: head.to_string(),
        git_tree: tree.id().to_string(),
    })
}

fn current_heads(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
) -> CliResult<BridgeSnapshot> {
    let head = git
        .head()
        .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
    let oid = head
        .target()
        .ok_or_else(|| git_error("Git HEAD does not point directly to a commit"))?;
    let tree = head
        .peel_to_commit()
        .and_then(|commit| commit.tree())
        .map_err(|error| git_error(format!("cannot read Git HEAD tree: {error}")))?;
    let view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    let atomic_state = repo
        .get_view_info(&view)
        .map_err(CliError::from)?
        .state
        .to_string();
    Ok(BridgeSnapshot {
        view,
        atomic_state,
        git_head: oid.to_string(),
        git_tree: tree.id().to_string(),
    })
}

pub(crate) fn read_checkpoint_observation(
    root: &Path,
) -> CliResult<Option<BridgeCheckpointObservation>> {
    read_workspace_metadata(root).map(|checkpoint| {
        checkpoint.map(|checkpoint| BridgeCheckpointObservation {
            view: checkpoint.view,
            atomic_state: checkpoint.atomic_state,
            git_head: checkpoint.git_head,
            git_tree: checkpoint.git_tree,
        })
    })
}

fn read_workspace_metadata(root: &Path) -> CliResult<Option<BridgeCheckpoint>> {
    checkpoint::read_checkpoint(root).map_err(checkpoint_error)
}

fn open_git(root: &Path) -> CliResult<GitRepository> {
    GitRepository::open(root).map_err(|error| git_error(format!("cannot open repository: {error}")))
}

fn current_attached_git_branch(git: &GitRepository) -> CliResult<String> {
    match observe_head(git).map_err(observation_error)? {
        HeadObservation::Attached { symref, .. } => symref
            .strip_prefix("refs/heads/")
            .map(str::to_string)
            .ok_or_else(|| git_error(format!("Git HEAD symref '{symref}' is not a local branch"))),
        HeadObservation::Detached { .. } => {
            Err(git_error("Git HEAD must be attached to a local branch"))
        }
        HeadObservation::Unborn { symref } => {
            Err(git_error(format!("Git HEAD is unborn at '{symref}'")))
        }
        HeadObservation::MissingTarget { symref } => Err(git_error(format!(
            "Git HEAD target '{symref}' does not exist"
        ))),
    }
}

fn require_clean_git_worktree(git: &GitRepository) -> CliResult<()> {
    let index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    if index.has_conflicts() {
        return Err(git_error("Git index contains unresolved conflicts"));
    }

    let mut options = GitStatusOptions::new();
    options
        .include_untracked(false)
        .recurse_untracked_dirs(false);
    let statuses = git
        .statuses(Some(&mut options))
        .map_err(|error| git_error(format!("cannot inspect Git status: {error}")))?;
    if statuses.iter().any(|entry| tracked_delta(entry.status())) {
        return Err(git_error(
            "Git has staged or unstaged changes to tracked paths",
        ));
    }
    Ok(())
}

fn require_matching_clean_workspaces<'repo>(
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &'repo GitRepository,
    require_atomic_clean: bool,
) -> CliResult<(String, git2::Oid, git2::Tree<'repo>)> {
    let branch = current_attached_git_branch(git)?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    if branch != desired_view {
        return Err(git_error(format!(
            "Git branch '{branch}' does not match current Atomic view '{desired_view}'"
        )));
    }

    require_clean_git_worktree(git)?;

    if require_atomic_clean {
        let atomic_status = repo
            .status(working_copy, StatusOptions::default())
            .map_err(CliError::from)?;
        if !atomic_status.is_clean() {
            return Err(git_error("Atomic working copy is not clean"));
        }
    }

    let head = git
        .head()
        .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
    let oid = head
        .target()
        .ok_or_else(|| git_error("Git HEAD does not point directly to a commit"))?;
    let tree = head
        .peel_to_commit()
        .and_then(|commit| commit.tree())
        .map_err(|error| git_error(format!("cannot read Git HEAD tree: {error}")))?;
    Ok((branch, oid, tree))
}

fn project_atomic_to_git(
    root: &Path,
    repo: &Repository,
    working_copy: WorkingCopyId,
    git: &GitRepository,
    current: &BridgeSnapshot,
) -> CliResult<()> {
    let head = git
        .head()
        .map_err(|error| git_error(format!("Git HEAD is unavailable: {error}")))?;
    if !head.is_branch() {
        return Err(git_error("Git HEAD must be attached to a local branch"));
    }
    let branch = head
        .shorthand()
        .ok_or_else(|| git_error("Git branch name is not valid UTF-8"))?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::from)?;
    if branch != desired_view {
        return Err(git_error(format!(
            "Git branch '{branch}' does not match current Atomic view '{desired_view}'"
        )));
    }

    let atomic_status = repo
        .status(working_copy, StatusOptions::default())
        .map_err(CliError::from)?;
    if !atomic_status.is_clean() {
        return Err(git_error("Atomic working copy is not clean"));
    }
    require_clean_git_index(git)?;

    // Atomic status proved that the current filesystem is the materialization
    // of this view. The final RFC replaces this MVP bridge with ProjectTree
    // computed directly from GraphVisibilityClosure.
    let filesystem_files = read_filesystem(root)?;
    let tree_oid = write_git_tree(git, &filesystem_files)?;
    let tree = git
        .find_tree(tree_oid)
        .map_err(|error| git_error(format!("cannot read projected Git tree: {error}")))?;
    let parent = head
        .peel_to_commit()
        .map_err(|error| git_error(format!("cannot read Git HEAD commit: {error}")))?;
    let signature = git
        .signature()
        .map_err(|error| git_error(format!("cannot determine Git signature: {error}")))?;
    let message = format!(
        "Atomic bridge projection\n\nAtomic-View: {}\nAtomic-State: {}\n",
        current.view, current.atomic_state
    );
    git.commit(
        Some("HEAD"),
        &signature,
        &signature,
        &message,
        &tree,
        &[&parent],
    )
    .map_err(|error| git_error(format!("cannot create Atomic projection commit: {error}")))?;

    let mut index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    index
        .read_tree(&tree)
        .and_then(|_| index.write())
        .map_err(|error| git_error(format!("cannot reset Git index to projected tree: {error}")))?;

    Ok(())
}

fn require_clean_git_index(git: &GitRepository) -> CliResult<()> {
    let index = git
        .index()
        .map_err(|error| git_error(format!("cannot read Git index: {error}")))?;
    if index.has_conflicts() {
        return Err(git_error("Git index contains unresolved conflicts"));
    }
    let mut options = GitStatusOptions::new();
    options
        .include_untracked(false)
        .recurse_untracked_dirs(false);
    let statuses = git
        .statuses(Some(&mut options))
        .map_err(|error| git_error(format!("cannot inspect Git status: {error}")))?;
    if statuses.iter().any(|entry| staged_delta(entry.status())) {
        return Err(git_error("Git index has staged changes"));
    }
    Ok(())
}

fn staged_delta(status: Status) -> bool {
    status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE
            | Status::CONFLICTED,
    )
}

#[derive(Default)]
struct GitTreeNode {
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeMap<String, GitTreeNode>,
}

fn write_git_tree(git: &GitRepository, files: &BTreeMap<String, Vec<u8>>) -> CliResult<Oid> {
    let mut root = GitTreeNode::default();
    for (path, content) in files {
        let mut components = path.split('/').peekable();
        let mut node = &mut root;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                node.files.insert(component.to_string(), content.clone());
            } else {
                node = node.directories.entry(component.to_string()).or_default();
            }
        }
    }
    write_git_tree_node(git, &root)
}

fn write_git_tree_node(git: &GitRepository, node: &GitTreeNode) -> CliResult<Oid> {
    let mut builder = git
        .treebuilder(None)
        .map_err(|error| git_error(format!("cannot create Git tree builder: {error}")))?;
    for (name, content) in &node.files {
        let oid = git
            .blob(content)
            .map_err(|error| git_error(format!("cannot create Git blob '{name}': {error}")))?;
        builder
            .insert(name, oid, 0o100644)
            .map_err(|error| git_error(format!("cannot add Git blob '{name}': {error}")))?;
    }
    for (name, child) in &node.directories {
        let oid = write_git_tree_node(git, child)?;
        builder
            .insert(name, oid, 0o040000)
            .map_err(|error| git_error(format!("cannot add Git tree '{name}': {error}")))?;
    }
    builder
        .write()
        .map_err(|error| git_error(format!("cannot write Git tree: {error}")))
}

fn read_filesystem(root: &Path) -> CliResult<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    collect_filesystem(root, root, &mut files)?;
    Ok(files)
}

fn collect_filesystem(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> CliResult<()> {
    let entries = fs::read_dir(directory).map_err(|error| {
        git_error(format!(
            "cannot read working tree directory '{}': {error}",
            directory.display()
        ))
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| git_error(format!("cannot read working tree: {error}")))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("filesystem traversal remains below repository root");
        let relative = path_to_git_string(relative)?;
        if excluded_path(&relative) {
            continue;
        }
        let kind = entry
            .file_type()
            .map_err(|error| git_error(format!("cannot inspect '{relative}': {error}")))?;
        if kind.is_dir() {
            collect_filesystem(root, &path, files)?;
        } else if kind.is_file() {
            let content = fs::read(&path)
                .map_err(|error| git_error(format!("cannot read '{relative}': {error}")))?;
            files.insert(relative, content);
        } else {
            return Err(git_error(format!(
                "unsupported working tree entry '{relative}'"
            )));
        }
    }
    Ok(())
}

fn path_to_git_string(path: &Path) -> CliResult<String> {
    let components: Result<Vec<_>, _> = path
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| git_error("working tree contains a non-UTF-8 path"))
        })
        .collect();
    Ok(components?.join("/"))
}

fn tracked_delta(status: Status) -> bool {
    status.intersects(
        Status::INDEX_NEW
            | Status::INDEX_MODIFIED
            | Status::INDEX_DELETED
            | Status::INDEX_RENAMED
            | Status::INDEX_TYPECHANGE
            | Status::WT_MODIFIED
            | Status::WT_DELETED
            | Status::WT_RENAMED
            | Status::WT_TYPECHANGE
            | Status::CONFLICTED,
    )
}

fn read_git_tree(
    git: &GitRepository,
    tree: &git2::Tree<'_>,
) -> CliResult<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    collect_git_tree(git, tree, "", &mut files)?;
    Ok(files)
}

fn collect_git_tree(
    git: &GitRepository,
    tree: &git2::Tree<'_>,
    prefix: &str,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> CliResult<()> {
    for entry in tree.iter() {
        let name = entry
            .name()
            .ok_or_else(|| git_error("Git tree contains a non-UTF-8 path"))?;
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        if excluded_path(&path) {
            continue;
        }

        match (entry.kind(), entry.filemode()) {
            (Some(ObjectType::Tree), 0o040000) => {
                let child = git.find_tree(entry.id()).map_err(|error| {
                    git_error(format!("cannot read Git tree '{path}': {error}"))
                })?;
                collect_git_tree(git, &child, &path, files)?;
            }
            (Some(ObjectType::Blob), 0o100644 | 0o100755) => {
                let blob = git.find_blob(entry.id()).map_err(|error| {
                    git_error(format!("cannot read Git blob '{path}': {error}"))
                })?;
                files.insert(path, blob.content().to_vec());
            }
            _ => {
                return Err(git_error(format!(
                    "unsupported Git entry '{path}' with mode {:o}",
                    entry.filemode()
                )));
            }
        }
    }
    Ok(())
}

fn excluded_path(path: &str) -> bool {
    path == ".git"
        || path.starts_with(".git/")
        || path == ".atomicignore"
        || path == ".atomic"
        || path.starts_with(".atomic/")
        || path == ".vault"
        || path.starts_with(".vault/")
}

fn compare_file_sets(
    git: &BTreeMap<String, Vec<u8>>,
    atomic: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let missing: Vec<_> = git
        .keys()
        .filter(|path| !atomic.contains_key(*path))
        .collect();
    let extra: Vec<_> = atomic
        .keys()
        .filter(|path| !git.contains_key(*path))
        .collect();
    let changed: Vec<_> = git
        .iter()
        .filter_map(|(path, content)| {
            atomic
                .get(path)
                .filter(|other| *other != content)
                .map(|_| path)
        })
        .collect();

    if missing.is_empty() && extra.is_empty() && changed.is_empty() {
        return Ok(());
    }

    Err(format!(
        "Git HEAD and Atomic view differ (missing in Atomic: {}; extra in Atomic: {}; different bytes: {})",
        format_paths(&missing),
        format_paths(&extra),
        format_paths(&changed)
    ))
}

fn format_paths<T: AsRef<str>>(paths: &[&T]) -> String {
    if paths.is_empty() {
        "none".to_string()
    } else {
        paths
            .iter()
            .map(|path| path.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn write_workspace_metadata(root: &Path, snapshot: &BridgeSnapshot) -> CliResult<()> {
    checkpoint::write_verified_checkpoint(
        root,
        VerifiedCheckpointInput {
            view: &snapshot.view,
            atomic_state: &snapshot.atomic_state,
            git_head: &snapshot.git_head,
            git_tree: &snapshot.git_tree,
        },
    )
    .map(|_| ())
    .map_err(checkpoint_error)
}

fn checkpoint_error(error: checkpoint::CheckpointError) -> CliError {
    git_error(error.to_string())
}

fn observation_error(error: ObservationError) -> CliError {
    git_error(error.to_string())
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Signature;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "atomic-bridge-collision-{}-{unique}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn paths(entries: &[&str]) -> BTreeSet<String> {
        entries.iter().map(|path| (*path).to_string()).collect()
    }

    fn files(entries: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(path, content)| ((*path).to_string(), content.to_vec()))
            .collect()
    }

    fn init_git_with_commit(root: &Path, branch: &str) {
        let repository = GitRepository::init(root).unwrap();
        repository
            .set_head(&format!("refs/heads/{branch}"))
            .unwrap();
        fs::write(root.join("tracked.txt"), b"tracked\n").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repository.find_tree(tree_oid).unwrap();
        let signature = Signature::now("Atomic Test", "atomic@example.com").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
    }

    #[test]
    fn checkpoint_refresh_skips_intentional_import_view_mismatch() {
        let root = TestDirectory::new();
        let repo = Repository::init_with_view(&root.0, "main").unwrap();
        drop(repo);
        init_git_with_commit(&root.0, "topic");

        assert_eq!(
            refresh_checkpoint_if_aligned(&root.0).unwrap(),
            CheckpointRefresh::SkippedViewMismatch
        );
        assert!(!checkpoint::checkpoint_path(&root.0).exists());
    }

    #[test]
    fn direction_classifier_detects_no_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-1", "atomic-1"),
            ReconcileDirection::Neither
        );
    }

    #[test]
    fn direction_classifier_detects_git_only_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-2", "atomic-1"),
            ReconcileDirection::GitToAtomic
        );
    }

    #[test]
    fn direction_classifier_detects_branch_only_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "topic", "git-1", "atomic-1"),
            ReconcileDirection::GitToAtomic
        );
    }

    #[test]
    fn direction_classifier_detects_atomic_only_change() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-1", "atomic-2"),
            ReconcileDirection::AtomicToGit
        );
    }

    #[test]
    fn direction_classifier_detects_divergence() {
        assert_eq!(
            classify_direction("main", "git-1", "atomic-1", "main", "git-2", "atomic-2"),
            ReconcileDirection::Diverged
        );
    }

    #[test]
    fn checkpoint_match_requires_every_recorded_head() {
        let checkpoint = BridgeCheckpoint::legacy_compatible("main", "atomic-1", "git-1", "tree-1");
        let mut snapshot = BridgeSnapshot {
            view: "main".to_string(),
            atomic_state: "atomic-1".to_string(),
            git_head: "git-1".to_string(),
            git_tree: "tree-1".to_string(),
        };
        assert!(checkpoint_matches_snapshot(&checkpoint, &snapshot));
        snapshot.git_tree = "tree-2".to_string();
        assert!(!checkpoint_matches_snapshot(&checkpoint, &snapshot));
    }

    #[test]
    fn comparison_accepts_identical_paths_and_bytes() {
        let git = files(&[("README.md", b"same"), ("src/main.rs", b"fn main() {}")]);
        assert_eq!(compare_file_sets(&git, &git), Ok(()));
    }

    #[test]
    fn comparison_reports_path_and_byte_differences() {
        let git = files(&[("changed", b"git"), ("missing", b"value")]);
        let atomic = files(&[("changed", b"atomic"), ("extra", b"value")]);
        let error = compare_file_sets(&git, &atomic).unwrap_err();
        assert!(error.contains("missing"));
        assert!(error.contains("extra"));
        assert!(error.contains("changed"));
    }

    #[test]
    fn collision_planner_allows_new_target_under_existing_directories() {
        let root = TestDirectory::new();
        fs::create_dir(root.0.join("src")).unwrap();

        assert_eq!(
            plan_switch_collisions(&root.0, &paths(&["README.md"]), &paths(&["src/lib.rs"])),
            Ok(())
        );
    }

    #[test]
    fn collision_planner_rejects_file_at_new_target_path() {
        let root = TestDirectory::new();
        fs::write(root.0.join("new.txt"), b"untracked").unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["new.txt"])).unwrap_err();
        assert!(error.contains("new.txt"));
        assert!(error.contains("occupied"));
    }

    #[test]
    fn collision_planner_rejects_directory_at_new_target_file_path() {
        let root = TestDirectory::new();
        fs::create_dir(root.0.join("config")).unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["config"])).unwrap_err();
        assert!(error.contains("config"));
        assert!(error.contains("directory"));
    }

    #[test]
    fn collision_planner_rejects_non_directory_parent() {
        let root = TestDirectory::new();
        fs::write(root.0.join("src"), b"not a directory").unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["src/lib.rs"])).unwrap_err();
        assert!(error.contains("src/lib.rs"));
        assert!(error.contains("non-directory parent 'src'"));
    }

    #[cfg(unix)]
    #[test]
    fn collision_planner_rejects_symlink_at_new_target_path() {
        use std::os::unix::fs::symlink;

        let root = TestDirectory::new();
        symlink("missing", root.0.join("linked")).unwrap();

        let error =
            plan_switch_collisions(&root.0, &BTreeSet::new(), &paths(&["linked"])).unwrap_err();
        assert!(error.contains("linked"));
        assert!(error.contains("occupied"));
    }

    #[test]
    fn collision_planner_ignores_paths_already_tracked_by_current_head() {
        let root = TestDirectory::new();
        fs::write(root.0.join("tracked.txt"), b"current").unwrap();

        assert_eq!(
            plan_switch_collisions(&root.0, &paths(&["tracked.txt"]), &paths(&["tracked.txt"])),
            Ok(())
        );
    }

    #[test]
    fn bridge_exclusions_are_exact_or_recursive() {
        assert!(excluded_path(".atomicignore"));
        assert!(excluded_path(".atomic/data"));
        assert!(excluded_path(".vault/intents/x"));
        assert!(!excluded_path("src/.atomic/file"));
        assert!(!excluded_path(".atomicignore.example"));
    }
}

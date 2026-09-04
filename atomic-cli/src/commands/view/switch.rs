//! The `view switch` command for switching between views.
//!
//! This module implements the `atomic view switch` command, which changes
//! the current view to a different one and updates the working copy to
//! match the new view's state.
//!
//! # Working Copy Update
//!
//! Like Pijul's channel switching, switching views in Atomic updates the
//! working copy to reflect the new view's state. Files are created, updated,
//! or removed to match what's recorded in the target view.
//!
//! # Usage
//!
//! ```text
//! atomic view switch <NAME>
//!
//! Arguments:
//!   <NAME>  Name of the view to switch to
//!
//! Options:
//!   -h, --help  Print help information
//! ```
//!
//! # Examples
//!
//! Switch to a different view:
//! ```text
//! $ atomic view switch feature-auth
//! Switched to view: feature-auth
//! ```
//!
//! Switch back to dev:
//! ```text
//! $ atomic view switch dev
//! Switched to view: dev
//! ```

use clap::Parser;
use clap_complete::engine::ArgValueCompleter;

use crate::commands::complete::complete_view_names;
use crate::commands::git::guard::{
    guard_working_copy, GuardError, GuardOperation, GuardOutcome, GuardRequest,
};

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, view as style_view};

#[cfg(test)]
use std::path::PathBuf;

// Switch Command

/// Switch to a different view.
///
/// Changes the current view to the specified one and updates the working
/// copy to match the new view's state. This behavior matches Pijul's
/// channel switching.
///
/// **Note**: Switching views WILL update your working copy files to match
/// the target view's state. Unrecorded changes may be overwritten.
#[derive(Parser, Debug, Default)]
#[command(name = "switch")]
pub struct Switch {
    /// Name of the view to switch to.
    ///
    /// The view must already exist. Use `atomic view list` to see
    /// available views, or `atomic view create` to create a new one.
    #[arg(value_name = "NAME", add = ArgValueCompleter::new(complete_view_names))]
    pub name: Option<String>,

    /// Force switch even with unrecorded changes.
    ///
    /// By default, switching views is blocked when the working copy has
    /// uncommitted changes (modified, added, or deleted files). This
    /// prevents accidental loss of work during materialization.
    ///
    /// Use `--force` to override this check. Unrecorded changes will
    /// be overwritten by the target view's content.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Stash unrecorded changes before switching.
    ///
    /// Automatically runs `atomic stash push` to save your uncommitted
    /// changes, then switches views. Use `atomic stash pop` after
    /// switching back to restore your changes.
    #[arg(long, short = 's')]
    pub stash: bool,
}

impl Switch {
    /// Create a new Switch command targeting the given view.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            force: false,
            stash: false,
        }
    }
}

fn checkpoint_shadow_switch(
    shadow_sync: crate::commands::git::shadow::ShadowSwitchSync,
    repo_root: &std::path::Path,
    view: &str,
) -> CliResult<()> {
    let shadow_was_synchronized = shadow_sync.is_synchronized();
    let checkpoint = match crate::commands::git::bridge::refresh_checkpoint_if_aligned(repo_root) {
        Ok(outcome) => outcome,
        Err(error) => {
            return match shadow_sync.rollback(repo_root) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(CliError::GitError {
                    message: format!("{error}; {rollback_error}"),
                }),
            };
        }
    };
    if shadow_was_synchronized
        && checkpoint != crate::commands::git::bridge::CheckpointRefresh::Refreshed
    {
        let error = CliError::GitError {
            message: format!(
                "Git shadow synchronization for view '{view}' completed, but the bridge verifier did not refresh its checkpoint ({checkpoint:?}); run `atomic git bridge verify` for details"
            ),
        };
        return match shadow_sync.rollback(repo_root) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(CliError::GitError {
                message: format!("{error}; {rollback_error}"),
            }),
        };
    }
    Ok(())
}

impl Command for Switch {
    fn run(&self) -> CliResult<()> {
        // Get the view name
        let name = self
            .name
            .as_ref()
            .ok_or_else(|| CliError::InvalidArgument {
                message: "View name is required".to_string(),
            })?;

        // Find the repository
        let repo_root = find_repository_root()?;

        match guard_working_copy(GuardRequest::new(&repo_root, GuardOperation::ViewSwitch))
            .map_err(|error| match error {
                GuardError::Checkpoint(error) => CliError::InvalidRepository {
                    reason: error.to_string(),
                },
                GuardError::Observation(error) => CliError::GitError {
                    message: error.to_string(),
                },
                GuardError::Wip(error) => CliError::GitError {
                    message: error.to_string(),
                },
            })? {
            GuardOutcome::Pass(_) => {}
            GuardOutcome::Refuse(refusal) => {
                return Err(CliError::StaleBaseline {
                    report: refusal.to_string(),
                });
            }
        }

        let mut repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;
        let working_copy = repo
            .require_working_copy_id()
            .map_err(CliError::Repository)?;
        let desired_view = repo
            .desired_view_name(working_copy)
            .map_err(CliError::Repository)?;

        // A prior coordinated switch may have materialized this view but failed
        // before Git/checkpoint alignment. In an active shadow repo, retry the
        // projection even when Atomic already points at the requested view; do
        // not print a false success while Git evidence is still stale.
        if desired_view == name.as_str() {
            let shadow_sync =
                crate::commands::git::shadow::sync_git_head_to_view(&repo, &repo_root, name)?;
            if !shadow_sync.is_synchronized() {
                print_success(&format!("Already on view: {}", style_view(name)));
                return Ok(());
            }
            drop(repo);
            checkpoint_shadow_switch(shadow_sync, &repo_root, name)?;
            print_success(&format!("Already on view: {}", style_view(name)));
            return Ok(());
        }

        // Block switch if working copy has unrecorded changes
        if !self.force {
            let status = repo
                .status(working_copy, atomic_repository::StatusOptions::default())
                .map_err(CliError::Repository)?;

            if !status.is_clean() {
                if self.stash {
                    // Auto-stash: save changes before switching
                    let stash_cmd = crate::commands::stash::Stash::new()
                        .with_message(format!("Auto-stash before switching to {}", name));
                    stash_cmd.run_push_on(&mut repo, None, false, false)?;
                } else {
                    let dirty: Vec<String> = status
                        .entries()
                        .iter()
                        .filter(|e| e.status().is_dirty())
                        .map(|e| format!("  {} {}", e.status().short_code(), e.path().display()))
                        .collect();

                    crate::output::print_error(&format!(
                        "Cannot switch views with unrecorded changes ({} file{}):",
                        dirty.len(),
                        if dirty.len() == 1 { "" } else { "s" },
                    ));
                    for line in &dirty {
                        eprintln!("{}", line);
                    }
                    eprintln!();
                    eprintln!("Use 'atomic record' to save changes, 'atomic view switch --stash' to stash them, or '--force' to discard them.");
                    return Err(CliError::InvalidArgument {
                        message: "Working copy has unrecorded changes".to_string(),
                    });
                }
            }
        }

        // Switch to the view and update working copy
        let spinner = crate::output::create_spinner("Materializing files for view...");

        let result = repo.switch_view(working_copy, name).map_err(|e| match e {
            atomic_repository::RepositoryError::ViewNotFound { name } => {
                CliError::ViewNotFound { name }
            }
            other => CliError::Repository(other),
        })?;

        // Git shadows Atomic: validate the just-materialized target through the
        // single shadow staging path, advance/create its mirror projection, and
        // align HEAD plus the live index without touching the working copy.
        // Active shadow sync is coordinated and fallible; plain no-Git and
        // non-shadow repositories remain no-ops.
        let shadow_sync =
            match crate::commands::git::shadow::sync_git_head_to_view(&repo, &repo_root, name) {
                Ok(outcome) => outcome,
                Err(error) => {
                    crate::output::finish_error(
                        &spinner,
                        "View materialized, but Git shadow synchronization failed",
                    );
                    return Err(error);
                }
            };

        if result.has_conflicts() {
            crate::output::print_warning(&format!(
                "{} conflicts detected",
                result.conflict_count()
            ));
        }

        // Release the original redb handle before the full bridge verifier
        // reopens the repository. A synchronized shadow switch must produce a
        // fresh checkpoint; no-Git and non-shadow switches preserve their
        // existing no-op behavior.
        drop(repo);
        if let Err(error) = checkpoint_shadow_switch(shadow_sync, &repo_root, name) {
            crate::output::finish_error(
                &spinner,
                "View and Git shadow aligned, but checkpoint refresh failed",
            );
            return Err(error);
        }

        crate::output::finish_success(
            &spinner,
            &format!(
                "Switched to view: {} ({} files updated, {} directories)",
                style_view(name),
                result.files_written,
                result.directories_created,
            ),
        );

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use std::process::Command as ProcessCommand;

    use super::*;
    use serial_test::serial;

    // -------------------------------------------------------------------------
    // Directory Guard for Safe Current Dir Changes
    // -------------------------------------------------------------------------

    struct DirGuard {
        original: PathBuf,
    }

    impl DirGuard {
        fn new() -> Self {
            Self {
                original: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn git_ok(root: &std::path::Path, args: &[&str]) -> String {
        let output = ProcessCommand::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    // -------------------------------------------------------------------------
    // Command Builder Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_switch_with_name() {
        let cmd = Switch::with_name("feature-auth");
        assert_eq!(cmd.name, Some("feature-auth".to_string()));
    }

    #[test]
    fn test_default() {
        let cmd = Switch::default();
        assert!(cmd.name.is_none());
    }

    // -------------------------------------------------------------------------
    // Error Handling Tests (without repository)
    // -------------------------------------------------------------------------

    #[test]
    fn test_run_without_name() {
        let cmd = Switch::default();
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::InvalidArgument { message } => {
                assert!(message.contains("required"));
            }
            other => panic!("Expected InvalidArgument, got: {:?}", other),
        }
    }

    // -------------------------------------------------------------------------
    // Integration Tests (require temp repository)
    // -------------------------------------------------------------------------

    #[test]
    #[serial]
    fn test_switch_to_existing_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and create a view, then drop to release lock
        {
            let mut repo = Repository::init(repo_path).unwrap();
            repo.create_view("feature-test").unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Switch to the new view
        let cmd = Switch::with_name("feature-test");
        let result = cmd.run();
        assert!(result.is_ok());
        assert!(
            !repo_path.join(".atomic/bridge/workspace.json").exists(),
            "a no-Git switch must not create bridge metadata"
        );

        // Verify we switched
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_view(), "feature-test");
    }

    #[test]
    #[serial]
    fn native_colocated_switch_projects_parent_advance_into_child_shadow() {
        let _guard = DirGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root, &["config", "user.name", "Atomic Test"]);
        git_ok(root, &["config", "user.email", "atomic@example.com"]);
        std::fs::write(root.join("tracked.txt"), b"tracked\n").unwrap();
        git_ok(root, &["add", "tracked.txt"]);
        git_ok(root, &["commit", "-q", "-m", "initial"]);
        std::env::set_current_dir(root).unwrap();

        crate::commands::git::Import {
            no_vault: true,
            ..crate::commands::git::Import::default()
        }
        .run()
        .unwrap();
        {
            let mut repo = Repository::open(root).unwrap();
            repo.create_view_from("feature", "main").unwrap();
        }

        let mut switch_feature = Switch::with_name("feature");
        switch_feature.force = true;
        switch_feature.run().unwrap();
        let stale_feature_tip = git_ok(root, &["rev-parse", "refs/heads/feature"]);

        let mut switch_main = Switch::with_name("main");
        switch_main.force = true;
        switch_main.run().unwrap();
        std::fs::write(root.join("tracked.txt"), b"tracked\nparent advance\n").unwrap();
        {
            let repo = Repository::open(root).unwrap();
            let working_copy = repo.require_working_copy_id().unwrap();
            repo.record(
                working_copy,
                atomic_core::change::ChangeHeader::new("advance shared parent"),
                atomic_repository::RecordOptions::new(),
            )
            .unwrap();
        }

        // The draft's own state did not advance, but its live parent filter did.
        // Switching it must project the inherited bytes instead of restoring the
        // stale feature branch/index from before the parent change.
        switch_feature.run().unwrap();
        let projected_feature_tip = git_ok(root, &["rev-parse", "HEAD"]);
        assert_ne!(projected_feature_tip, stale_feature_tip);
        assert_eq!(git_ok(root, &["rev-parse", "HEAD^"]), stale_feature_tip);
        assert_eq!(git_ok(root, &["branch", "--show-current"]), "feature");
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
            "tracked\nparent advance\n"
        );
        assert_eq!(
            git_ok(root, &["show", "HEAD:tracked.txt"]),
            "tracked\nparent advance"
        );

        let head_tree = git_ok(root, &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(git_ok(root, &["write-tree"]), head_tree);
        assert!(git_ok(root, &["status", "--porcelain=v1"]).is_empty());
        let message = git_ok(root, &["log", "-1", "--format=%B"]);
        assert!(message.contains("Atomic-View: feature"));
        assert!(message.contains("Atomic-State: "));
        assert!(!message.contains("Atomic-Changes:"));

        let checkpoint = crate::commands::git::checkpoint::read_checkpoint(root)
            .unwrap()
            .expect("coordinated switch must refresh checkpoint v2");
        assert_eq!(checkpoint.view, "feature");
        assert_eq!(checkpoint.git_head, projected_feature_tip);
        assert_eq!(checkpoint.git_tree, head_tree);
        assert_eq!(
            checkpoint.git_index_tree.as_deref(),
            Some(head_tree.as_str())
        );
        let reopened = Repository::open(root).unwrap();
        let working_copy = reopened.require_working_copy_id().unwrap();
        assert!(reopened
            .status(working_copy, atomic_repository::StatusOptions::default())
            .unwrap()
            .is_clean());
        drop(reopened);

        // Revisiting a target whose tip already has the materialized tree must
        // reuse that tip rather than manufacturing another projection commit.
        switch_main.run().unwrap();
        switch_feature.run().unwrap();
        assert_eq!(git_ok(root, &["rev-parse", "HEAD"]), projected_feature_tip);
        assert!(git_ok(root, &["status", "--porcelain=v1"]).is_empty());

        std::fs::write(root.join("new.txt"), b"new\n").unwrap();
        crate::commands::add::Add::new()
            .with_files(["new.txt"])
            .run()
            .expect("add guard must accept refreshed checkpoint");
        crate::commands::status::Status::new()
            .with_short(true)
            .run()
            .expect("status guard must accept refreshed checkpoint");
    }

    #[test]
    #[serial]
    fn conflicted_shadow_projection_fails_and_restores_git_evidence() {
        let _guard = DirGuard::new();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root, &["config", "user.name", "Atomic Test"]);
        git_ok(root, &["config", "user.email", "atomic@example.com"]);
        std::fs::write(root.join("tracked.txt"), b"base\n").unwrap();
        git_ok(root, &["add", "tracked.txt"]);
        git_ok(root, &["commit", "-q", "-m", "initial"]);
        std::env::set_current_dir(root).unwrap();

        crate::commands::git::Import {
            no_vault: true,
            ..crate::commands::git::Import::default()
        }
        .run()
        .unwrap();
        {
            let mut repo = Repository::open(root).unwrap();
            repo.create_view_from("feature", "main").unwrap();
        }
        let mut switch_feature = Switch::with_name("feature");
        switch_feature.force = true;
        switch_feature.run().unwrap();
        std::fs::write(
            root.join("tracked.txt"),
            b">>>>>>> feature\nfeature\n=======\nmain\n<<<<<<< main\n",
        )
        .unwrap();
        {
            let repo = Repository::open(root).unwrap();
            let working_copy = repo.require_working_copy_id().unwrap();
            repo.record(
                working_copy,
                atomic_core::change::ChangeHeader::new("record unresolved projection"),
                atomic_repository::RecordOptions::new().allow_conflict_markers(true),
            )
            .unwrap();
        }

        let mut switch_main = Switch::with_name("main");
        switch_main.force = true;
        switch_main.run().unwrap();
        let original_head = git_ok(root, &["rev-parse", "HEAD"]);
        let original_feature = git_ok(root, &["rev-parse", "refs/heads/feature"]);
        let original_index = git_ok(root, &["write-tree"]);

        let error = switch_feature
            .run()
            .expect_err("conflicted materialization must not become Git evidence");
        assert!(error.to_string().contains("conflict marker"));
        assert_eq!(git_ok(root, &["branch", "--show-current"]), "main");
        assert_eq!(git_ok(root, &["rev-parse", "HEAD"]), original_head);
        assert_eq!(
            git_ok(root, &["rev-parse", "refs/heads/feature"]),
            original_feature
        );
        assert_eq!(git_ok(root, &["write-tree"]), original_index);
        assert!(git_ok(root, &["status", "--porcelain=v1"]).contains("tracked.txt"));
        let checkpoint = crate::commands::git::checkpoint::read_checkpoint(root)
            .unwrap()
            .expect("failed projection must preserve the prior checkpoint");
        assert_eq!(checkpoint.view, "main");
        assert_eq!(Repository::open(root).unwrap().current_view(), "feature");

        // Retrying the already-current Atomic view after resolving the conflict
        // must repair Git/checkpoint alignment instead of returning early.
        std::fs::write(root.join("tracked.txt"), b"resolved\n").unwrap();
        {
            let repo = Repository::open(root).unwrap();
            let working_copy = repo.require_working_copy_id().unwrap();
            repo.record(
                working_copy,
                atomic_core::change::ChangeHeader::new("resolve projection"),
                atomic_repository::RecordOptions::new(),
            )
            .unwrap();
        }
        switch_feature.run().unwrap();
        assert_eq!(git_ok(root, &["branch", "--show-current"]), "feature");
        assert_eq!(git_ok(root, &["show", "HEAD:tracked.txt"]), "resolved");
        assert!(git_ok(root, &["status", "--porcelain=v1"]).is_empty());
        assert_eq!(
            crate::commands::git::checkpoint::read_checkpoint(root)
                .unwrap()
                .unwrap()
                .view,
            "feature"
        );
    }

    #[test]
    #[serial]
    fn test_switch_to_nonexistent_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Try to switch to a non-existent view
        let cmd = Switch::with_name("nonexistent");
        let result = cmd.run();
        assert!(result.is_err());
        match result.unwrap_err() {
            CliError::ViewNotFound { name } => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("Expected ViewNotFound, got: {:?}", other),
        }
    }

    #[test]
    #[serial]
    fn test_switch_to_current_view() {
        use tempfile::tempdir;

        let _guard = DirGuard::new();
        let temp = tempdir().unwrap();
        let repo_path = temp.path();

        // Initialize a repository (default view is "dev") and drop to release lock
        {
            let _repo = Repository::init(repo_path).unwrap();
        }

        // Change to the repo directory
        std::env::set_current_dir(repo_path).unwrap();

        // Switch to the current view (should succeed with a message)
        let cmd = Switch::with_name("dev");
        let result = cmd.run();
        assert!(result.is_ok());

        // Verify we're still on dev
        let repo = Repository::open(repo_path).unwrap();
        assert_eq!(repo.current_view(), "dev");
    }
}

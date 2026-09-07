//! Ordered, mode-aware workspace transaction entry protocol.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::operation::OperationScope;
use atomic_core::pristine::{ViewState, ViewTxnT, WorkingCopyRecord, WorkingCopyTxnT};
use atomic_core::{OperationId, WorkingCopyId};
use serde::Deserialize;

use super::git_observation::{
    observe_git_metadata, GitHeadObservation, GitObservationToken, GitOperationMarker,
    WorkspaceGitObservation,
};
use super::locks::WorkingCopyOperationLockGuard;
use super::{OperationHeadState, Repository};
use crate::RepositoryError;

/// Maximum number of stable-observation attempts at one transaction boundary.
pub const MAX_WORKSPACE_TXN_ATTEMPTS: u8 = 3;
/// A workspace entry plan always contains exactly these three ordered phases.
pub const MAX_WORKSPACE_ENTRY_PLAN_ITEMS: usize = 3;
const CHECKPOINT_RELATIVE_PATH: &str = ".atomic/bridge/workspace.json";

/// Mutation policy for a workspace transaction boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceTxnMode {
    /// Reconcile safe drift and refuse unsafe states.
    Reconcile,
    /// Report state without domain mutation.
    Observe,
    /// Permit explicit repair, except while Git owns a sequence operation.
    Force,
}

/// Result of entering a workspace transaction boundary.
pub enum WorkspaceTxnStart {
    /// The observed workspace is stable and ready for command-specific work.
    Ready(WorkspaceTxn),
    /// The workspace is validly observed but requires a typed corrective action.
    Remediation(WorkspaceRemediation),
}

/// A stable workspace boundary retaining the canonical operation locks.
pub struct WorkspaceTxn {
    _operation_lock: WorkingCopyOperationLockGuard,
    mode: WorkspaceTxnMode,
    attempts: u8,
    record: WorkingCopyRecord,
    view: ViewState,
    operation_heads: OperationHeadState,
    checkpoint: Option<WorkspaceCheckpoint>,
    git: WorkspaceGitObservation,
    plan: WorkspaceEntryPlan,
}

impl WorkspaceTxn {
    pub fn mode(&self) -> WorkspaceTxnMode {
        self.mode
    }

    pub fn attempts(&self) -> u8 {
        self.attempts
    }

    pub fn working_copy(&self) -> WorkingCopyId {
        self.record.id
    }

    pub fn working_copy_record(&self) -> &WorkingCopyRecord {
        &self.record
    }

    pub fn view(&self) -> &ViewState {
        &self.view
    }

    pub fn operation_heads(&self) -> &OperationHeadState {
        &self.operation_heads
    }

    pub fn checkpoint(&self) -> Option<&WorkspaceCheckpoint> {
        self.checkpoint.as_ref()
    }

    pub fn git(&self) -> &WorkspaceGitObservation {
        &self.git
    }

    pub fn plan(&self) -> &WorkspaceEntryPlan {
        &self.plan
    }
}

/// Fixed-size entry phases. Their field order is the execution order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceEntryPlan {
    head: WorkspaceHeadPlan,
    filesystem: WorkspaceFilesystemPlan,
    refs: WorkspaceRefPlan,
}

impl WorkspaceEntryPlan {
    fn aligned() -> Self {
        Self {
            head: WorkspaceHeadPlan::Aligned,
            filesystem: WorkspaceFilesystemPlan::ObserveBaseline,
            refs: WorkspaceRefPlan::ObserveMapped,
        }
    }

    fn blocked(checkpoint: Option<String>, observed: GitHeadObservation) -> Self {
        Self {
            head: WorkspaceHeadPlan::ReconcileBeforeFilesystem {
                checkpoint,
                observed,
            },
            filesystem: WorkspaceFilesystemPlan::BlockedUntilHeadAligned,
            refs: WorkspaceRefPlan::Deferred,
        }
    }

    pub fn head(&self) -> &WorkspaceHeadPlan {
        &self.head
    }

    pub fn filesystem(&self) -> WorkspaceFilesystemPlan {
        self.filesystem
    }

    pub fn refs(&self) -> WorkspaceRefPlan {
        self.refs
    }

    pub fn item_count(&self) -> usize {
        MAX_WORKSPACE_ENTRY_PLAN_ITEMS
    }

    pub fn is_ordered(&self) -> bool {
        matches!(
            (&self.head, &self.filesystem),
            (
                WorkspaceHeadPlan::Aligned,
                WorkspaceFilesystemPlan::ObserveBaseline
            ) | (
                WorkspaceHeadPlan::ReconcileBeforeFilesystem { .. },
                WorkspaceFilesystemPlan::BlockedUntilHeadAligned
            )
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceHeadPlan {
    Aligned,
    ReconcileBeforeFilesystem {
        checkpoint: Option<String>,
        observed: GitHeadObservation,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceFilesystemPlan {
    ObserveBaseline,
    BlockedUntilHeadAligned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceRefPlan {
    ObserveMapped,
    Deferred,
}

/// Mode-independent corrective information returned instead of unsafe mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceRemediation {
    GitOperationInProgress {
        mode: WorkspaceTxnMode,
        repository_state: String,
        markers: Vec<GitOperationMarker>,
        conflict_stages: Vec<u8>,
        disposition: GitOperationDisposition,
    },
    Unanchored {
        mode: WorkspaceTxnMode,
        state: UnanchoredWorkspace,
        plan: WorkspaceEntryPlan,
    },
    OperationHeadsDiverged {
        heads: Vec<OperationId>,
    },
    ConcurrentGitMutation {
        attempts: u8,
        first: GitObservationToken,
        last: GitObservationToken,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitOperationDisposition {
    ObserveOnly,
    FinishOrAbortInGit,
    ForceForbidden,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnanchoredWorkspace {
    DetachedHead {
        oid: String,
    },
    UnbornHead {
        symref: String,
    },
    MissingHeadTarget {
        symref: String,
    },
    MissingCheckpoint,
    GitRepositoryMissing,
    AtomicCheckpointDrift {
        checkpoint_view: String,
        checkpoint_state: String,
        desired_view: String,
        desired_state: String,
    },
    HeadSymrefChanged {
        checkpoint: String,
        observed: String,
    },
    HeadTreeChanged {
        checkpoint: String,
        observed: String,
    },
    IndexTreeChanged {
        checkpoint: String,
        observed: Option<String>,
    },
    HeadChanged {
        checkpoint: String,
        observed: String,
    },
    IndexLocked {
        path: PathBuf,
    },
}

/// Checkpoint fields needed by the entry protocol. Reading never rewrites legacy data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceCheckpoint {
    pub version: u32,
    pub view: String,
    pub atomic_state: String,
    pub git_head_symref: Option<String>,
    pub git_head: String,
    pub git_tree: String,
    pub git_index_tree: Option<String>,
    pub git_index_digest: Option<String>,
}

#[derive(Deserialize)]
struct CheckpointWire {
    version: u32,
    view: String,
    atomic_state: String,
    #[serde(default)]
    git_head_symref: Option<String>,
    git_head: String,
    git_tree: String,
    #[serde(default)]
    git_index_tree: Option<String>,
    #[serde(default)]
    git_index_digest: Option<String>,
}

impl Repository {
    /// Enter the shared workspace boundary while retaining its ordered locks.
    pub fn begin_workspace_txn(
        &mut self,
        mode: WorkspaceTxnMode,
    ) -> Result<WorkspaceTxnStart, RepositoryError> {
        self.begin_workspace_txn_with(mode, observe_git_metadata)
    }

    fn begin_workspace_txn_with<F>(
        &mut self,
        mode: WorkspaceTxnMode,
        mut observe: F,
    ) -> Result<WorkspaceTxnStart, RepositoryError>
    where
        F: FnMut(&Path) -> Result<WorkspaceGitObservation, super::ObservationError>,
    {
        let working_copy = self.require_working_copy_id()?;
        let operation_lock = self.try_lock_workspace_operation(working_copy)?;
        let (mut record, mut view) = self.load_workspace_authority(working_copy)?;
        let observed_operation_heads = self
            .operation_log(OperationScope::WorkingCopy(working_copy), Some(0), false)?
            .head_state;
        if mode == WorkspaceTxnMode::Observe {
            if let OperationHeadState::Diverged(heads) = &observed_operation_heads {
                return Ok(WorkspaceTxnStart::Remediation(
                    WorkspaceRemediation::OperationHeadsDiverged {
                        heads: heads.clone(),
                    },
                ));
            }
        }

        let checkpoint = read_workspace_checkpoint(self.root())?;
        let mut first_changed = None;
        let mut last_changed = None;

        for attempt in 1..=MAX_WORKSPACE_TXN_ATTEMPTS {
            let initial = observe(self.root()).map_err(observation_error)?;
            if let Some(remediation) =
                classify_git_state(mode, &record, &view, checkpoint.as_ref(), &initial)
            {
                return Ok(WorkspaceTxnStart::Remediation(remediation));
            }

            let plan = WorkspaceEntryPlan::aligned();
            debug_assert!(plan.is_ordered());
            debug_assert!(plan.item_count() <= MAX_WORKSPACE_ENTRY_PLAN_ITEMS);

            let final_observation = observe(self.root()).map_err(observation_error)?;
            let initial_token = initial.token();
            let final_token = final_observation.token();
            if initial_token != final_token {
                if first_changed.is_none() {
                    first_changed = Some(initial_token);
                }
                last_changed = Some(final_token);
                continue;
            }
            if let Some(remediation) = classify_git_state(
                mode,
                &record,
                &view,
                checkpoint.as_ref(),
                &final_observation,
            ) {
                return Ok(WorkspaceTxnStart::Remediation(remediation));
            }

            let (operation_heads, final_observation) = match mode {
                WorkspaceTxnMode::Observe => (observed_operation_heads.clone(), final_observation),
                WorkspaceTxnMode::Reconcile | WorkspaceTxnMode::Force => {
                    self.recover_pending_deferred_tree_alignment_locked(&operation_lock)?;
                    self.ensure_repository_operation_safe_for(&operation_lock)?;
                    if let OperationHeadState::Diverged(heads) =
                        self.consolidate_operation_heads_locked(&operation_lock)?
                    {
                        return Ok(WorkspaceTxnStart::Remediation(
                            WorkspaceRemediation::OperationHeadsDiverged { heads },
                        ));
                    }
                    self.recover_incomplete_operation(&operation_lock)?;
                    (record, view) = self.load_workspace_authority(working_copy)?;

                    let recovered_observation = observe(self.root()).map_err(observation_error)?;
                    if recovered_observation.token() != final_token {
                        return Ok(WorkspaceTxnStart::Remediation(
                            WorkspaceRemediation::ConcurrentGitMutation {
                                attempts: attempt,
                                first: final_token,
                                last: recovered_observation.token(),
                            },
                        ));
                    }
                    if let Some(remediation) = classify_git_state(
                        mode,
                        &record,
                        &view,
                        checkpoint.as_ref(),
                        &recovered_observation,
                    ) {
                        return Ok(WorkspaceTxnStart::Remediation(remediation));
                    }
                    (
                        self.consolidate_operation_heads_locked(&operation_lock)?,
                        recovered_observation,
                    )
                }
            };
            if let OperationHeadState::Diverged(heads) = &operation_heads {
                return Ok(WorkspaceTxnStart::Remediation(
                    WorkspaceRemediation::OperationHeadsDiverged {
                        heads: heads.clone(),
                    },
                ));
            }

            return Ok(WorkspaceTxnStart::Ready(WorkspaceTxn {
                _operation_lock: operation_lock,
                mode,
                attempts: attempt,
                record,
                view,
                operation_heads,
                checkpoint,
                git: final_observation,
                plan,
            }));
        }

        Ok(WorkspaceTxnStart::Remediation(
            WorkspaceRemediation::ConcurrentGitMutation {
                attempts: MAX_WORKSPACE_TXN_ATTEMPTS,
                first: first_changed.expect("a failed retry records its initial token"),
                last: last_changed.expect("a failed retry records its final token"),
            },
        ))
    }

    fn load_workspace_authority(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<(WorkingCopyRecord, ViewState), RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let record = txn
            .get_working_copy(working_copy)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
        let view = txn
            .get_view_by_id(record.desired_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: format!("id {}", record.desired_view),
            })?;
        if view.state != record.desired_state {
            return Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "working-copy {} expects view '{}' at {}, but the view is at {}",
                    working_copy, view.name, record.desired_state, view.state
                ),
            });
        }
        Ok((record, view))
    }
}

fn classify_git_state(
    mode: WorkspaceTxnMode,
    record: &WorkingCopyRecord,
    view: &ViewState,
    checkpoint: Option<&WorkspaceCheckpoint>,
    observation: &WorkspaceGitObservation,
) -> Option<WorkspaceRemediation> {
    let WorkspaceGitObservation::Repository(git) = observation else {
        return checkpoint.map(|checkpoint| {
            unanchored(
                mode,
                UnanchoredWorkspace::GitRepositoryMissing,
                Some(checkpoint),
                GitHeadObservation::Unborn {
                    symref: "no-git".to_string(),
                },
            )
        });
    };

    let conflict_stages = git.conflict_stages();
    if git.operation.is_in_progress() || !conflict_stages.is_empty() {
        let disposition = match mode {
            WorkspaceTxnMode::Observe => GitOperationDisposition::ObserveOnly,
            WorkspaceTxnMode::Reconcile => GitOperationDisposition::FinishOrAbortInGit,
            WorkspaceTxnMode::Force => GitOperationDisposition::ForceForbidden,
        };
        return Some(WorkspaceRemediation::GitOperationInProgress {
            mode,
            repository_state: git.operation.repository_state.clone(),
            markers: git.operation.present_markers(),
            conflict_stages,
            disposition,
        });
    }

    if git.index_lock.is_present() {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::IndexLocked {
                path: git.index_lock.path.clone(),
            },
            checkpoint,
            git.head.clone(),
        ));
    }

    let observed_oid = match &git.head {
        GitHeadObservation::Attached { oid, .. } => oid,
        GitHeadObservation::Detached { oid } => {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::DetachedHead { oid: oid.clone() },
                checkpoint,
                git.head.clone(),
            ))
        }
        GitHeadObservation::Unborn { symref } => {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::UnbornHead {
                    symref: symref.clone(),
                },
                checkpoint,
                git.head.clone(),
            ))
        }
        GitHeadObservation::MissingTarget { symref } => {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::MissingHeadTarget {
                    symref: symref.clone(),
                },
                checkpoint,
                git.head.clone(),
            ))
        }
    };

    let Some(checkpoint) = checkpoint else {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::MissingCheckpoint,
            None,
            git.head.clone(),
        ));
    };
    let atomic_checkpoint_drift = checkpoint.view != view.name;
    if atomic_checkpoint_drift && mode != WorkspaceTxnMode::Force {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::AtomicCheckpointDrift {
                checkpoint_view: checkpoint.view.clone(),
                checkpoint_state: checkpoint.atomic_state.clone(),
                desired_view: view.name.clone(),
                desired_state: record.desired_state.to_string(),
            },
            Some(checkpoint),
            git.head.clone(),
        ));
    }
    if let (Some(checkpoint_symref), Some(observed_symref)) =
        (checkpoint.git_head_symref.as_ref(), head_symref(&git.head))
    {
        if checkpoint_symref != observed_symref {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::HeadSymrefChanged {
                    checkpoint: checkpoint_symref.clone(),
                    observed: observed_symref.to_string(),
                },
                Some(checkpoint),
                git.head.clone(),
            ));
        }
    }
    if checkpoint.git_head != *observed_oid {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::HeadChanged {
                checkpoint: checkpoint.git_head.clone(),
                observed: observed_oid.clone(),
            },
            Some(checkpoint),
            git.head.clone(),
        ));
    }
    if git.head_tree.as_deref() != Some(checkpoint.git_tree.as_str()) {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::HeadTreeChanged {
                checkpoint: checkpoint.git_tree.clone(),
                observed: git.head_tree.clone().unwrap_or_default(),
            },
            Some(checkpoint),
            git.head.clone(),
        ));
    }
    if let Some(checkpoint_index_tree) = &checkpoint.git_index_tree {
        if git.index_tree.as_ref() != Some(checkpoint_index_tree) {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::IndexTreeChanged {
                    checkpoint: checkpoint_index_tree.clone(),
                    observed: git.index_tree.clone(),
                },
                Some(checkpoint),
                git.head.clone(),
            ));
        }
    }
    None
}

fn head_symref(head: &GitHeadObservation) -> Option<&str> {
    match head {
        GitHeadObservation::Attached { symref, .. }
        | GitHeadObservation::Unborn { symref }
        | GitHeadObservation::MissingTarget { symref } => Some(symref),
        GitHeadObservation::Detached { .. } => None,
    }
}

fn unanchored(
    mode: WorkspaceTxnMode,
    state: UnanchoredWorkspace,
    checkpoint: Option<&WorkspaceCheckpoint>,
    observed: GitHeadObservation,
) -> WorkspaceRemediation {
    WorkspaceRemediation::Unanchored {
        mode,
        state,
        plan: WorkspaceEntryPlan::blocked(
            checkpoint.map(|checkpoint| checkpoint.git_head.clone()),
            observed,
        ),
    }
}

fn read_workspace_checkpoint(root: &Path) -> Result<Option<WorkspaceCheckpoint>, RepositoryError> {
    let path = root.join(CHECKPOINT_RELATIVE_PATH);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "cannot read bridge checkpoint '{}': {error}",
                    path.display()
                ),
            })
        }
    };
    let wire: CheckpointWire =
        serde_json::from_slice(&bytes).map_err(|error| RepositoryError::InvalidRepository {
            reason: format!(
                "bridge checkpoint '{}' is malformed: {error}",
                path.display()
            ),
        })?;
    if wire.version != 1 && wire.version != 2 {
        return Err(RepositoryError::InvalidRepository {
            reason: format!("unsupported bridge checkpoint version {}", wire.version),
        });
    }
    let git_head_symref = wire
        .git_head_symref
        .or_else(|| (wire.version == 1).then(|| format!("refs/heads/{}", wire.view)));
    Ok(Some(WorkspaceCheckpoint {
        version: wire.version,
        view: wire.view,
        atomic_state: wire.atomic_state,
        git_head_symref,
        git_head: wire.git_head,
        git_tree: wire.git_tree.clone(),
        git_index_tree: wire
            .git_index_tree
            .or_else(|| (wire.version == 1).then_some(wire.git_tree)),
        git_index_digest: wire.git_index_digest,
    }))
}

fn observation_error(error: super::ObservationError) -> RepositoryError {
    RepositoryError::InvalidRepository {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::Hash;

    fn head(oid: &str) -> GitHeadObservation {
        GitHeadObservation::Attached {
            symref: "refs/heads/main".to_string(),
            oid: oid.to_string(),
        }
    }

    #[test]
    fn bounded_plan_structurally_blocks_filesystem_before_head_alignment() {
        let plan = WorkspaceEntryPlan::blocked(Some("old".to_string()), head("new"));
        assert!(plan.is_ordered());
        assert_eq!(plan.item_count(), 3);
        assert!(matches!(
            plan.filesystem,
            WorkspaceFilesystemPlan::BlockedUntilHeadAligned
        ));
    }

    #[test]
    fn observation_token_compares_head_and_index() {
        let first = token("one", b"index-one");
        let second = token("one", b"index-two");
        assert_ne!(first, second);
    }

    #[test]
    fn unstable_observation_retries_three_times_then_returns_remediation() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let mut observations = vec![
            fake_git("stable", b"one"),
            fake_git("changed", b"two"),
            fake_git("stable", b"one"),
            fake_git("changed", b"three"),
            fake_git("stable", b"one"),
            fake_git("changed", b"four"),
        ]
        .into_iter();

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| {
                Ok(observations.next().unwrap())
            })
            .unwrap();
        let WorkspaceTxnStart::Remediation(WorkspaceRemediation::ConcurrentGitMutation {
            attempts,
            ..
        }) = start
        else {
            panic!("unstable observations must return retry remediation");
        };
        assert_eq!(attempts, MAX_WORKSPACE_TXN_ATTEMPTS);
    }

    #[test]
    fn changed_first_attempt_is_discarded_and_second_stable_attempt_succeeds() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let mut observations = vec![
            fake_git("stable", b"one"),
            fake_git("changed", b"two"),
            fake_git("stable", b"three"),
            fake_git("stable", b"three"),
        ]
        .into_iter();

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| {
                Ok(observations.next().unwrap())
            })
            .unwrap();
        let WorkspaceTxnStart::Ready(txn) = start else {
            panic!("the second stable attempt should succeed");
        };
        assert_eq!(txn.attempts(), 2);
    }

    #[test]
    fn unsafe_state_appearing_on_reobservation_never_returns_ready() {
        use super::super::git_observation::{GitAdminEntryKind, GitOperationMarkerObservation};

        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let clean = fake_git("stable", b"one");
        let mut sequence = clean.clone();
        let WorkspaceGitObservation::Repository(git) = &mut sequence else {
            unreachable!();
        };
        git.operation.markers.push(GitOperationMarkerObservation {
            marker: GitOperationMarker::RebaseApply,
            path: PathBuf::from(".git/rebase-apply"),
            kind: GitAdminEntryKind::Directory,
        });
        let mut observations = vec![clean, sequence.clone(), sequence].into_iter();

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| {
                Ok(observations.next().unwrap())
            })
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
        ));
    }

    #[test]
    fn conflicted_index_stages_are_git_owned_and_cannot_be_forced() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let mut conflict = fake_git("stable", b"conflict");
        let WorkspaceGitObservation::Repository(git) = &mut conflict else {
            unreachable!();
        };
        git.index_stages = vec![1, 2, 3];

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Force, |_| Ok(conflict.clone()))
            .unwrap();
        let WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
            conflict_stages,
            disposition,
            ..
        }) = start
        else {
            panic!("conflicted index must return Git-owned remediation");
        };
        assert_eq!(conflict_stages, vec![1, 2, 3]);
        assert_eq!(disposition, GitOperationDisposition::ForceForbidden);
    }

    fn fake_git(oid: &str, index: &[u8]) -> WorkspaceGitObservation {
        use super::super::git_observation::{
            GitAdminEntryKind, GitAdminPathObservation, GitOperationObservation,
            WorkspaceGitRepositoryObservation,
        };

        WorkspaceGitObservation::Repository(Box::new(WorkspaceGitRepositoryObservation {
            worktree_git_dir: PathBuf::from(".git"),
            common_dir: PathBuf::from(".git"),
            index_path: PathBuf::from(".git/index"),
            head: head(oid),
            head_tree: Some("tree".to_string()),
            index_digest: Hash::of(index),
            index_tree: Some("tree".to_string()),
            index_stages: vec![0],
            index_lock: GitAdminPathObservation {
                path: PathBuf::from(".git/index.lock"),
                kind: GitAdminEntryKind::Missing,
            },
            operation: GitOperationObservation {
                repository_state: "Clean".to_string(),
                markers: Vec::new(),
            },
        }))
    }

    fn token(oid: &str, index: &[u8]) -> GitObservationToken {
        GitObservationToken {
            head: head(oid),
            head_tree: Some("tree".to_string()),
            index_digest: Hash::of(index),
            index_tree: Some("tree".to_string()),
            index_stages: vec![0],
            index_locked: false,
            repository_state: "Clean".to_string(),
            markers: Vec::new(),
        }
    }

    fn write_test_checkpoint(root: &Path, repo: &Repository, oid: &str) {
        let path = root.join(CHECKPOINT_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let state = repo
            .working_copy_record(repo.require_working_copy_id().unwrap())
            .unwrap()
            .desired_state;
        fs::write(
            path,
            format!(
                "{{\"version\":2,\"view\":\"dev\",\"atomic_state\":\"{state}\",\"git_head\":\"{oid}\",\"git_tree\":\"tree\"}}"
            ),
        )
        .unwrap();
    }
}

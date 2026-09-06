use super::*;
use atomic_core::change::{CausalFrontier, ChangeKind, ChangeOrigin};
use atomic_core::operation::{
    ActorRef, EffectPlan, EffectTarget, EffectValue, FileKind, FileState, OperationKind,
    RepoStateRef,
};

/// Repository state for a working copy's private snapshot view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotState {
    pub view: String,
    pub baseline_view: String,
    pub head: Option<Hash>,
    pub remainder: Option<Hash>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotStatus {
    pub view: String,
    pub baseline_view: String,
    pub snapshot: Option<Hash>,
    pub remainder: Option<Hash>,
    pub superseded_snapshots: usize,
}

impl SnapshotStatus {
    pub fn is_active(&self) -> bool {
        self.snapshot.is_some() || self.remainder.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotRetentionPolicy {
    pub keep_superseded: usize,
}

impl Default for SnapshotRetentionPolicy {
    fn default() -> Self {
        Self { keep_superseded: 8 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotRetentionOutcome {
    pub retained: Vec<Hash>,
    pub deleted: Vec<Hash>,
}

#[derive(Clone, Debug)]
pub(super) enum RecordLifecycle {
    Durable,
    Snapshot {
        working_copy: WorkingCopyId,
        view: String,
        supersedes: Option<Hash>,
        replaces: Option<Hash>,
    },
    Promotion {
        snapshot_view: String,
        snapshot: Hash,
    },
}

impl RecordLifecycle {
    pub(super) fn target_view<'a>(&'a self, baseline: &'a str) -> &'a str {
        match self {
            Self::Durable | Self::Promotion { .. } => baseline,
            Self::Snapshot { view, .. } => view,
        }
    }

    pub(super) fn removal(&self) -> Option<(&str, Hash)> {
        match self {
            Self::Durable => None,
            Self::Snapshot {
                view,
                replaces: Some(previous),
                ..
            } => Some((view, *previous)),
            Self::Snapshot { replaces: None, .. } => None,
            Self::Promotion {
                snapshot_view,
                snapshot,
            } => Some((snapshot_view, *snapshot)),
        }
    }

    pub(super) fn classify(&self, change: Change) -> Result<Change, RecordError> {
        let (kind, supersedes) = match self {
            Self::Durable | Self::Promotion { .. } => (ChangeKind::Durable, None),
            Self::Snapshot {
                working_copy,
                supersedes,
                ..
            } => (
                ChangeKind::Snapshot {
                    working_copy: *working_copy,
                },
                *supersedes,
            ),
        };
        change
            .with_classification(
                kind,
                supersedes,
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            )
            .map_err(|error| RecordError::ChangeStore(error.to_string()))
    }

    pub(super) fn verify_promotion_content(
        &self,
        change: &Change,
        repo: &Repository,
    ) -> Result<(), RecordError> {
        let Self::Promotion { snapshot, .. } = self else {
            return Ok(());
        };
        let snapshot_change = repo
            .load_change(snapshot)
            .map_err(RecordError::Repository)?;
        if change.hunks() != snapshot_change.hunks()
            || change.contents != snapshot_change.contents
            || change.hashed.file_ops != snapshot_change.hashed.file_ops
        {
            return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                message: format!(
                    "working copy no longer matches snapshot {}; create a new snapshot before promotion",
                    snapshot.to_base32()
                ),
            }));
        }
        Ok(())
    }
}

impl Repository {
    /// Canonical private view name for a physical working copy.
    pub fn snapshot_view_name(working_copy: WorkingCopyId) -> String {
        format!("wc/{working_copy}")
    }

    /// Inspect or create the private Draft snapshot view owned by `working_copy`.
    pub fn ensure_snapshot_view(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<SnapshotState, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let baseline_view = self.desired_view_name(working_copy)?;
        let view_name = Self::snapshot_view_name(working_copy);
        ensure_workspace_dir(&self.dot_dir, &view_name)?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let baseline = txn
            .get_view(&baseline_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: baseline_view.clone(),
            })?;
        let snapshot_view = match txn
            .get_view(&view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            Some(mut view) => {
                if !view.kind.is_draft() {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!("snapshot view '{}' must remain Draft", view_name),
                    });
                }
                if view.parent != Some(baseline.id) {
                    if view.change_count != 0 {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "snapshot view '{}' still contains a snapshot for a different baseline",
                                view_name
                            ),
                        });
                    }
                    view.parent = Some(baseline.id);
                    txn.update_view(&view)
                        .map_err(|error| RepositoryError::Database(error.to_string()))?;
                }
                view
            }
            None => txn
                .create_view(&view_name, ViewScope::Draft, Some(baseline.id))
                .map_err(|error| RepositoryError::Database(error.to_string()))?,
        };

        let mut direct = None;
        for row in txn
            .iter_changes(&snapshot_view, 0)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (_, node_id, _) =
                row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            let hash = txn
                .get_external(node_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: node_id.to_string(),
                })?;
            if direct.replace(hash).is_some() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "snapshot view '{}' contains more than one direct change",
                        view_name
                    ),
                });
            }
        }
        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;

        let mut head = None;
        let mut remainder = None;
        if let Some(hash) = direct {
            let change = self.load_change(&hash)?;
            if change.kind().is_snapshot() {
                if change.kind().working_copy() != Some(working_copy) {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "snapshot view '{}' contains change {} not owned by working copy {}",
                            view_name,
                            hash.to_base32(),
                            working_copy
                        ),
                    });
                }
                head = Some(hash);
            } else {
                remainder = Some(hash);
            }
        }

        Ok(SnapshotState {
            view: view_name,
            baseline_view,
            head,
            remainder,
        })
    }

    /// Record the complete working-copy delta relative to its durable baseline.
    pub fn snapshot(
        &self,
        working_copy: WorkingCopyId,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        if !options.get_save_to_store() || !options.get_apply_after_record() {
            return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                message: "snapshots must be saved and applied atomically".to_string(),
            }));
        }
        let state = self.ensure_snapshot_view(working_copy)?;
        self.record_with_lifecycle(
            working_copy,
            header,
            options,
            RecordLifecycle::Snapshot {
                working_copy,
                view: state.view,
                supersedes: state.head,
                replaces: state.head.or(state.remainder),
            },
        )
    }

    /// Promote the current snapshot by reassembling its content as a durable change.
    pub fn promote_snapshot(
        &self,
        working_copy: WorkingCopyId,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        if !options.get_save_to_store() || !options.get_apply_after_record() {
            return Err(RecordError::Repository(RepositoryError::InvalidOperation {
                message: "snapshot promotion must be saved and applied atomically".to_string(),
            }));
        }
        let state = self.ensure_snapshot_view(working_copy)?;
        let snapshot = state.head.ok_or_else(|| {
            RecordError::Repository(RepositoryError::InvalidOperation {
                message: format!("snapshot view '{}' has no snapshot to promote", state.view),
            })
        })?;
        self.record_with_lifecycle(
            working_copy,
            header,
            options,
            RecordLifecycle::Promotion {
                snapshot_view: state.view,
                snapshot,
            },
        )
    }

    /// Read snapshot/remainder lifecycle state without creating or mutating its private view.
    pub fn snapshot_status(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<SnapshotStatus, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let baseline_view = self.desired_view_name(working_copy)?;
        let view_name = Self::snapshot_view_name(working_copy);
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let Some(view) = txn
            .get_view(&view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        else {
            return Ok(SnapshotStatus {
                view: view_name,
                baseline_view,
                snapshot: None,
                remainder: None,
                superseded_snapshots: 0,
            });
        };
        let mut direct = Vec::new();
        for row in txn
            .iter_changes(&view, 0)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (_, node_id, _) =
                row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            direct.push(
                txn.get_external(node_id)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                    .ok_or_else(|| RepositoryError::ChangeNotFound {
                        hash: node_id.to_string(),
                    })?,
            );
        }
        drop(txn);
        if direct.len() > 1 {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "private view '{}' contains multiple direct changes",
                    view_name
                ),
            });
        }
        let mut snapshot = None;
        let mut remainder = None;
        if let Some(hash) = direct.first().copied() {
            if self.load_change(&hash)?.kind().is_snapshot() {
                snapshot = Some(hash);
            } else {
                remainder = Some(hash);
            }
        }
        let mut superseded_snapshots = 0;
        let mut cursor = snapshot
            .and_then(|hash| self.load_change(&hash).ok())
            .and_then(|change| change.supersedes().copied());
        let mut seen = std::collections::HashSet::new();
        while let Some(hash) = cursor {
            if !seen.insert(hash) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("snapshot supersedes cycle at {}", hash.to_base32()),
                });
            }
            superseded_snapshots += 1;
            cursor = self
                .load_change(&hash)
                .ok()
                .and_then(|change| change.supersedes().copied());
        }
        Ok(SnapshotStatus {
            view: view_name,
            baseline_view,
            snapshot,
            remainder,
            superseded_snapshots,
        })
    }

    /// Delete superseded snapshot objects beyond the retention window through leased effects.
    pub fn prune_superseded_snapshots(
        &self,
        working_copy: WorkingCopyId,
        policy: SnapshotRetentionPolicy,
    ) -> Result<SnapshotRetentionOutcome, RepositoryError> {
        let status = self.snapshot_status(working_copy)?;
        let Some(head) = status.snapshot else {
            return Ok(SnapshotRetentionOutcome::default());
        };
        let mut chain = Vec::new();
        let mut cursor = self.load_change(&head)?.supersedes().copied();
        let mut seen = std::collections::HashSet::new();
        while let Some(hash) = cursor {
            if !seen.insert(hash) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("snapshot supersedes cycle at {}", hash.to_base32()),
                });
            }
            let change = self.load_change(&hash)?;
            cursor = change.supersedes().copied();
            chain.push(hash);
        }
        let retained = chain
            .iter()
            .take(policy.keep_superseded)
            .copied()
            .collect::<Vec<_>>();
        let candidates = chain
            .iter()
            .skip(policy.keep_superseded)
            .copied()
            .filter(|hash| {
                self.views_containing_change(hash)
                    .map(|views| views.is_empty())
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Ok(SnapshotRetentionOutcome {
                retained,
                deleted: Vec::new(),
            });
        }

        let operation_lock = self.try_lock_operation(working_copy)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let record = self.working_copy_record(working_copy)?;
        let state = RepoStateRef {
            view: None,
            working_copy: Some(super::operation::working_copy_state_ref(record)),
            git: None,
        };
        let mut effects = Vec::with_capacity(candidates.len());
        for (ordinal, hash) in candidates.iter().enumerate() {
            let path = self.change_store().change_path(hash);
            let bytes = std::fs::read(&path)?;
            let relative = path
                .strip_prefix(&self.root)
                .map_err(|error| RepositoryError::InvalidOperation {
                    message: error.to_string(),
                })?
                .to_string_lossy()
                .replace('\\', "/");
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&path)?.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = 0o644;
            effects.push(EffectPlan {
                ordinal: ordinal as u32,
                target: EffectTarget::FilesystemPath { path: relative },
                expected_old: EffectValue::File(FileState {
                    kind: FileKind::Regular,
                    mode,
                    content: Hash::of(&bytes),
                }),
                expected_new: EffectValue::Absent,
            });
        }
        let prepared = self.prepare_working_copy_transition(
            &operation_lock,
            OperationKind::Record,
            None,
            state.clone(),
            state,
            effects,
            ActorRef::System {
                name: "repository-snapshot-retention".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;
        let operation_id = prepared.operation().id();
        let mut deleted = Vec::new();
        for (ordinal, hash) in candidates.iter().enumerate() {
            let pending = self.execute_filesystem_effect(
                &operation_lock,
                operation_id,
                ordinal as u32,
                None,
            )?;
            self.record_pending_filesystem_effect(&operation_lock, operation_id, pending)?;
            self.change_store().evict(hash);
            deleted.push(*hash);
        }
        self.finalize_operation_verified(&operation_lock, operation_id)?;
        Ok(SnapshotRetentionOutcome { retained, deleted })
    }

    pub(super) fn ensure_change_allowed_in_view<T: ViewTxnT>(
        &self,
        txn: &T,
        view_name: &str,
        change: &Change,
    ) -> Result<(), RepositoryError> {
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        if let Some(owner) = change.kind().working_copy() {
            if view.kind.is_shared() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("snapshot changes cannot enter Shared view '{}'", view_name),
                });
            }
            let owner_view = Self::snapshot_view_name(owner);
            if view_name != owner_view {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "snapshot owned by working copy {} can only enter private view '{}'",
                        owner, owner_view
                    ),
                });
            }
        }
        Ok(())
    }

    pub(super) fn ensure_exchangeable_change(&self, root: &Hash) -> Result<(), RepositoryError> {
        let mut pending = vec![*root];
        let mut seen = std::collections::HashSet::new();
        while let Some(hash) = pending.pop() {
            if !seen.insert(hash) {
                continue;
            }
            let change = self.load_change(&hash)?;
            if change.kind().is_snapshot() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "change {} is a private snapshot and cannot be exchanged",
                        hash.to_base32()
                    ),
                });
            }
            pending.extend(change.dependencies().iter().copied());
        }
        Ok(())
    }

    pub(super) fn ensure_view_has_no_snapshots<T: ViewTxnT>(
        &self,
        txn: &T,
        view: &atomic_core::pristine::ViewState,
    ) -> Result<(), RepositoryError> {
        for row in txn
            .iter_changes(view, 0)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (_, node_id, _) =
                row.map_err(|error| RepositoryError::Database(error.to_string()))?;
            let hash = txn
                .get_external(node_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: node_id.to_string(),
                })?;
            if self.load_change(&hash)?.kind().is_snapshot() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "view '{}' contains private snapshot {}; it cannot become Shared",
                        view.name,
                        hash.to_base32()
                    ),
                });
            }
        }
        Ok(())
    }
}

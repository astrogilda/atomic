use super::*;
use atomic_core::change::{CausalFrontier, ChangeKind, ChangeOrigin};

/// Repository state for a working copy's private snapshot view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotState {
    pub view: String,
    pub baseline_view: String,
    pub head: Option<Hash>,
}

#[derive(Clone, Debug)]
pub(super) enum RecordLifecycle {
    Durable,
    Snapshot {
        working_copy: WorkingCopyId,
        view: String,
        previous: Option<Hash>,
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
                previous: Some(previous),
                ..
            } => Some((view, *previous)),
            Self::Snapshot { previous: None, .. } => None,
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
                previous,
                ..
            } => (
                ChangeKind::Snapshot {
                    working_copy: *working_copy,
                },
                *previous,
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

        let mut head = None;
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
            if head.replace(hash).is_some() {
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

        if let Some(hash) = head {
            let change = self.load_change(&hash)?;
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
        }

        Ok(SnapshotState {
            view: view_name,
            baseline_view,
            head,
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
                previous: state.head,
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

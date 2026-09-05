//! Repository-level operation preparation and lease-safe recovery.
//!
//! This module deliberately owns orchestration rather than command routing. Callers
//! prepare and durably publish an immutable operation before performing any external
//! effect, record deterministic receipts after effects, and invoke recovery while the
//! same ordered per-working-copy operation lock is held.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use atomic_core::operation::{
    ActorRef, EffectPlan, EffectReceipt, EffectReceiptKind, EffectReceiptPayload, EffectTarget,
    EffectValue, FileKind, FileState, Operation, OperationKind, OperationPayload, OperationScope,
    RepoStateDelta, RepoStateRef, WorkingCopyStateRef,
};
use atomic_core::pristine::{OperationMutTxnT, OperationTxnT, WorkingCopyRecord, WorkingCopyTxnT};
use atomic_core::{Hash, OperationId, WorkingCopyId};

use super::locks::WorkingCopyOperationLockGuard;
use super::Repository;
use crate::RepositoryError;

const RECOVERY_DIR: &str = "operation-recovery";
const BACKUP_ENTRIES_DIR: &str = "entries";
const BACKUP_VALUE: &str = "value";
const BACKUP_ABSENT: &str = "absent";
const BACKUP_COMPLETE: &str = "complete";
const BACKUP_VERSION: &[u8] = b"atomic-operation-recovery-v1\n";
const RECEIPT_ATTEMPT: u32 = 0;
const ANCHOR_ACTOR: &str = "cb-1b-anchor";
const RECOVERY_ACTOR: &str = "cb-1b-recovery";

/// Pure classification of an observed value against one expected-old/new lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LeaseClassification {
    /// The old lease still holds and the effect may be applied.
    Apply,
    /// The new lease already holds; replay is an idempotent no-op.
    AlreadyApplied,
    /// Neither lease holds; touching the resource could overwrite newer work.
    Diverged,
}

/// Result of preparing a switch operation and its recovery substrate.
#[derive(Debug, Clone)]
pub(super) struct PreparedSwitchOperation {
    operation: Operation,
    backup_root: PathBuf,
}

impl PreparedSwitchOperation {
    pub(super) fn operation(&self) -> &Operation {
        &self.operation
    }

    pub(super) fn backup_root(&self) -> &Path {
        &self.backup_root
    }
}

/// A landed filesystem effect awaiting its immutable receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingFilesystemEffect {
    ordinal: u32,
    observed_before: EffectValue,
    observed_after: EffectValue,
    mutated: bool,
}

impl PendingFilesystemEffect {
    pub(super) fn mutated(&self) -> bool {
        self.mutated
    }
}

/// Outcome of checking or recovering the current operation head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecoveryOutcome {
    /// No operation has been prepared for this working copy yet.
    NoOperation,
    /// The current head already has an operation-level `Verified` receipt.
    AlreadyComplete { operation: OperationId },
    /// An inverse recovery operation was created or resumed and completed.
    Recovered {
        original: OperationId,
        recovery: OperationId,
        created: bool,
    },
}

#[derive(Debug)]
enum ResolvedRecoveryTarget {
    Filesystem {
        path: PathBuf,
        recursive_directory: bool,
    },
    WorkingCopy(WorkingCopyId),
}

/// Classify one observation without performing I/O or consulting receipts.
pub(super) fn filesystem_directory_value(mode: u32) -> EffectValue {
    EffectValue::File(FileState {
        kind: FileKind::Directory,
        mode,
        content: Hash::of(b"atomic:filesystem-directory-entry:v1\0"),
    })
}

pub(super) fn classify_effect_lease(
    observed: &EffectValue,
    expected_old: &EffectValue,
    expected_new: &EffectValue,
) -> LeaseClassification {
    if observed == expected_old {
        LeaseClassification::Apply
    } else if observed == expected_new {
        LeaseClassification::AlreadyApplied
    } else {
        LeaseClassification::Diverged
    }
}

/// Construct a stable content-addressed receipt for one operation.
///
/// Attempt and timestamp are derived from immutable operation data, so replaying the
/// same recovery decision produces the same receipt ID and append-only storage dedups it.
pub(super) fn deterministic_effect_receipt(
    operation: &Operation,
    effect_ordinal: Option<u32>,
    kind: EffectReceiptKind,
    observed_old: Option<EffectValue>,
    observed_new: Option<EffectValue>,
) -> Result<EffectReceipt, RepositoryError> {
    EffectReceipt::new(EffectReceiptPayload {
        operation: operation.id(),
        effect_ordinal,
        attempt: RECEIPT_ATTEMPT,
        kind,
        observed_old,
        observed_new,
        timestamp_ms: operation.payload().timestamp_ms,
    })
    .map_err(codec_error)
}

/// Whether a receipt set contains the immutable operation-level completion receipt.
pub(super) fn has_operation_verified_receipt(receipts: &[EffectReceipt]) -> bool {
    receipts.iter().any(|receipt| {
        receipt.payload().kind == EffectReceiptKind::Verified
            && receipt.payload().effect_ordinal.is_none()
    })
}

impl Repository {
    /// Ensure one per-working-copy anchor exists and return the sole current head.
    ///
    /// Multiple heads are intentionally refused until CB-1C adds consolidation and
    /// explicit divergence records.
    pub(super) fn ensure_working_copy_anchor(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        state: RepoStateRef,
    ) -> Result<OperationId, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        validate_state_working_copy(&state, working_copy)?;
        let scope = OperationScope::WorkingCopy(working_copy);
        let mut txn = operation_lock.begin_write_immediate()?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        match heads.as_slice() {
            [head] => Ok(*head),
            [] => {
                let anchor = Operation::new(OperationPayload {
                    parents: Vec::new(),
                    kind: OperationKind::Anchor,
                    working_copy: Some(working_copy),
                    before: state.clone(),
                    delta: RepoStateDelta {
                        after: state,
                        effects: Vec::new(),
                    },
                    git_observed: Vec::new(),
                    evidence: Vec::new(),
                    actor: ActorRef::System {
                        name: ANCHOR_ACTOR.to_string(),
                    },
                    timestamp_ms: 0,
                    lossy: Vec::new(),
                })
                .map_err(codec_error)?;
                txn.put_operation(&anchor).map_err(pristine_error)?;
                txn.compare_and_set_operation_heads(scope, &[], &[anchor.id()])
                    .map_err(pristine_error)?;
                let verified = deterministic_effect_receipt(
                    &anchor,
                    None,
                    EffectReceiptKind::Verified,
                    None,
                    None,
                )?;
                txn.append_effect_receipt(&verified)
                    .map_err(pristine_error)?;
                txn.commit()?;
                Ok(anchor.id())
            }
            many => Err(multiple_heads_error(scope, many)),
        }
    }

    /// Prepare and immediately persist a switch operation before external effects.
    ///
    /// Filesystem leases are preflighted, the operation/head CAS is fsync-durable,
    /// and only then are immutable old-value backups written. A returned value proves
    /// that the complete backup marker is durable and callers may start effects.
    pub(super) fn prepare_switch_operation(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        before: RepoStateRef,
        after: RepoStateRef,
        effects: Vec<EffectPlan>,
        actor: ActorRef,
        timestamp_ms: i64,
    ) -> Result<PreparedSwitchOperation, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        validate_state_working_copy(&before, working_copy)?;
        validate_state_working_copy(&after, working_copy)?;
        let parent = self.ensure_working_copy_anchor(operation_lock, before.clone())?;
        self.require_complete_single_head(working_copy, parent)?;

        let operation = Operation::new(OperationPayload {
            parents: vec![parent],
            kind: OperationKind::SwitchView,
            working_copy: Some(working_copy),
            before,
            delta: RepoStateDelta { after, effects },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor,
            timestamp_ms,
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        validate_effect_target_chains(&operation)?;

        self.preflight_filesystem_leases(working_copy, &operation)?;
        let scope = OperationScope::WorkingCopy(working_copy);
        let mut txn = operation_lock.begin_write_immediate()?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(scope, &[parent], &[operation.id()])
            .map_err(pristine_error)?;
        txn.commit()?;

        let backup_root = self.snapshot_operation_filesystem(operation_lock, &operation)?;
        Ok(PreparedSwitchOperation {
            operation,
            backup_root,
        })
    }

    /// Append a deterministic receipt after a caller performs or observes an effect.
    ///
    /// `observed_before` is classified against the immutable plan. The receipt is
    /// appended only when `observed_after` equals expected-new; third values append a
    /// stable rejection receipt and return a typed invalid-operation error.
    /// Execute one planned working-tree path transition under its exact lease.
    ///
    /// The returned outcome is deliberately separate from receipt persistence so
    /// crash tests can exercise the effect-to-receipt window. Normal callers must
    /// immediately pass it to [`Self::record_pending_filesystem_effect`].
    pub(super) fn execute_filesystem_effect(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
        effect_ordinal: u32,
        content: Option<&[u8]>,
    ) -> Result<PendingFilesystemEffect, RepositoryError> {
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(operation_lock, &operation)?;
        let effect = effect_at(&operation, effect_ordinal)?;
        if !matches!(effect.target, EffectTarget::FilesystemPath { .. }) {
            return Err(unsupported_effect_error(&effect.target));
        }
        let working_copy = operation_lock.working_copy();
        let target = self.resolve_recovery_target(working_copy, &effect.target)?;
        let observed_before = self.observe_recovery_target(&target)?;
        match classify_effect_lease(&observed_before, &effect.expected_old, &effect.expected_new) {
            LeaseClassification::AlreadyApplied => {
                return Ok(PendingFilesystemEffect {
                    ordinal: effect.ordinal,
                    observed_before: observed_before.clone(),
                    observed_after: observed_before,
                    mutated: false,
                });
            }
            LeaseClassification::Diverged => {
                return self
                    .record_effect_outcome(
                        operation_lock,
                        operation_id,
                        effect.ordinal,
                        observed_before.clone(),
                        observed_before,
                    )
                    .and_then(|_| {
                        Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "effect {} unexpectedly accepted a divergent filesystem lease",
                                effect.ordinal
                            ),
                        })
                    });
            }
            LeaseClassification::Apply => {}
        }

        let ResolvedRecoveryTarget::Filesystem { path, .. } = &target else {
            return Err(unsupported_effect_error(&effect.target));
        };
        match &effect.expected_new {
            EffectValue::Absent => remove_filesystem_effect_path(path)?,
            EffectValue::File(state) if state.kind == FileKind::Regular => {
                let bytes = content.ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "regular-file effect {} requires prepared content bytes",
                        effect.ordinal
                    ),
                })?;
                if Hash::of(bytes) != state.content {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "prepared content for effect {} does not match expected-new hash",
                            effect.ordinal
                        ),
                    });
                }
                write_atomic_regular(path, bytes, state.mode)?;
            }
            EffectValue::File(state) if state.kind == FileKind::Directory => {
                let staging = self
                    .working_copy_effect_recovery_root(working_copy, operation.id())
                    .join("staged-directories")
                    .join(format!("{:010}", effect.ordinal));
                create_directory_effect_path(path, state.mode, &staging)?;
            }
            value => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "filesystem effect {} cannot materialize expected-new value {value:?}",
                        effect.ordinal
                    ),
                })
            }
        }
        let observed_after = self.observe_recovery_target(&target)?;
        if observed_after != effect.expected_new {
            let _ = self.record_effect_outcome(
                operation_lock,
                operation_id,
                effect.ordinal,
                observed_before.clone(),
                observed_after.clone(),
            );
            return Err(lease_divergence_error(
                effect,
                &observed_before,
                Some(&observed_after),
            ));
        }
        Ok(PendingFilesystemEffect {
            ordinal: effect.ordinal,
            observed_before,
            observed_after,
            mutated: true,
        })
    }

    pub(super) fn record_pending_filesystem_effect(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
        pending: PendingFilesystemEffect,
    ) -> Result<bool, RepositoryError> {
        self.record_effect_outcome(
            operation_lock,
            operation_id,
            pending.ordinal,
            pending.observed_before,
            pending.observed_after,
        )?;
        Ok(pending.mutated)
    }

    pub(super) fn append_rejected_effect_outcome(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        operation: &Operation,
        effect_ordinal: u32,
        observed_before: EffectValue,
        observed_after: Option<EffectValue>,
    ) -> Result<(), RepositoryError> {
        effect_at(operation, effect_ordinal)?;
        let receipt = deterministic_effect_receipt(
            operation,
            Some(effect_ordinal),
            EffectReceiptKind::LeaseRejected,
            Some(observed_before),
            observed_after,
        )?;
        txn.append_effect_receipt(&receipt).map_err(pristine_error)
    }

    pub(super) fn append_successful_effect_outcome(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        operation: &Operation,
        effect_ordinal: u32,
        observed_before: EffectValue,
        observed_after: EffectValue,
    ) -> Result<(), RepositoryError> {
        let effect = effect_at(operation, effect_ordinal)?;
        let classification =
            classify_effect_lease(&observed_before, &effect.expected_old, &effect.expected_new);
        if classification == LeaseClassification::Diverged || observed_after != effect.expected_new
        {
            return Err(lease_divergence_error(
                effect,
                &observed_before,
                Some(&observed_after),
            ));
        }
        let kind = match classification {
            LeaseClassification::Apply => EffectReceiptKind::Applied,
            LeaseClassification::AlreadyApplied => EffectReceiptKind::Recovered,
            LeaseClassification::Diverged => unreachable!("handled above"),
        };
        let receipt = deterministic_effect_receipt(
            operation,
            Some(effect_ordinal),
            kind,
            Some(observed_before),
            Some(observed_after),
        )?;
        txn.append_effect_receipt(&receipt).map_err(pristine_error)
    }

    pub(super) fn record_effect_outcome(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
        effect_ordinal: u32,
        observed_before: EffectValue,
        observed_after: EffectValue,
    ) -> Result<EffectReceipt, RepositoryError> {
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(operation_lock, &operation)?;
        let effect = effect_at(&operation, effect_ordinal)?;
        let classification =
            classify_effect_lease(&observed_before, &effect.expected_old, &effect.expected_new);
        if classification == LeaseClassification::Diverged || observed_after != effect.expected_new
        {
            let receipt = deterministic_effect_receipt(
                &operation,
                Some(effect_ordinal),
                EffectReceiptKind::LeaseRejected,
                Some(observed_before.clone()),
                Some(observed_after.clone()),
            )?;
            self.append_receipt_immediate(operation_lock, &operation, &receipt)?;
            return Err(lease_divergence_error(
                effect,
                &observed_before,
                Some(&observed_after),
            ));
        }

        let kind = match classification {
            LeaseClassification::Apply => EffectReceiptKind::Applied,
            LeaseClassification::AlreadyApplied => EffectReceiptKind::Recovered,
            LeaseClassification::Diverged => unreachable!("handled above"),
        };
        let receipt = deterministic_effect_receipt(
            &operation,
            Some(effect_ordinal),
            kind,
            Some(observed_before),
            Some(observed_after),
        )?;
        self.append_receipt_immediate(operation_lock, &operation, &receipt)?;
        Ok(receipt)
    }

    /// Append the deterministic operation-level `Verified` receipt immediately.
    pub(super) fn finalize_operation_verified(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
    ) -> Result<EffectReceipt, RepositoryError> {
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(operation_lock, &operation)?;
        let receipts = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_effect_receipts(operation_id)
                .map_err(pristine_error)?
        };
        let completed: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::Applied
                | EffectReceiptKind::Recovered
                | EffectReceiptKind::RolledBack => receipt.payload().effect_ordinal,
                EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
            })
            .collect();
        for effect in &operation.payload().delta.effects {
            if !completed.contains(&effect.ordinal) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} cannot be verified before effect {} has a successful receipt",
                        operation.id(),
                        effect.ordinal
                    ),
                });
            }
        }
        let working_copy = operation_lock.working_copy();
        let mut final_targets = Vec::<(&EffectTarget, &EffectValue)>::new();
        for effect in &operation.payload().delta.effects {
            if let Some((_, value)) = final_targets
                .iter_mut()
                .find(|(target, _)| **target == effect.target)
            {
                *value = &effect.expected_new;
            } else {
                final_targets.push((&effect.target, &effect.expected_new));
            }
        }
        for (target, expected) in final_targets {
            let resolved = self.resolve_recovery_target(working_copy, target)?;
            let observed = self.observe_recovery_target(&resolved)?;
            if &observed != expected {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} cannot be verified: target {target:?} expected {expected:?}, observed {observed:?}",
                        operation.id()
                    ),
                });
            }
        }
        let receipt = deterministic_effect_receipt(
            &operation,
            None,
            EffectReceiptKind::Verified,
            None,
            None,
        )?;
        self.append_receipt_immediate(operation_lock, &operation, &receipt)?;
        Ok(receipt)
    }

    /// Test whether one operation has an immutable operation-level verified receipt.
    pub(super) fn operation_is_verified(
        &self,
        operation_id: OperationId,
    ) -> Result<bool, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let receipts = txn
            .get_effect_receipts(operation_id)
            .map_err(pristine_error)?;
        Ok(has_operation_verified_receipt(&receipts))
    }

    /// Return whether a working-copy head requires writable recovery.
    pub(super) fn working_copy_operation_requires_recovery(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<bool, RepositoryError> {
        let scope = OperationScope::WorkingCopy(working_copy);
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        if heads.as_slice().len() > 1 {
            return Ok(true);
        }
        let Some(head) = heads.as_slice().first().copied() else {
            return Ok(false);
        };
        let receipts = txn.get_effect_receipts(head).map_err(pristine_error)?;
        Ok(!has_operation_verified_receipt(&receipts))
    }

    /// Recover the sole incomplete operation head for one working copy.
    ///
    /// Incomplete non-Recover heads first receive an immutable inverse `Recover`
    /// child through an immediate head CAS. An incomplete Recover head is resumed in
    /// place. Filesystem effects are then replayed idempotently from the original
    /// operation's backups. Unsupported effects and third lease values fail closed.
    pub(super) fn recover_incomplete_operation(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
    ) -> Result<RecoveryOutcome, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        let scope = OperationScope::WorkingCopy(working_copy);
        let heads = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_operation_heads(scope).map_err(pristine_error)?
        };
        let head = match heads.as_slice() {
            [] => return Ok(RecoveryOutcome::NoOperation),
            [head] => *head,
            many => return Err(multiple_heads_error(scope, many)),
        };
        let head_operation = self.load_operation(head)?;
        self.validate_operation_lock(operation_lock, &head_operation)?;
        if self.operation_is_verified(head)? {
            return Ok(RecoveryOutcome::AlreadyComplete { operation: head });
        }

        let (original, recovery, created) =
            if head_operation.payload().kind == OperationKind::Recover {
                let original_id = sole_recovery_parent(&head_operation)?;
                (self.load_operation(original_id)?, head_operation, false)
            } else {
                let recovery = self.inverse_recovery_operation(&head_operation, working_copy)?;
                let mut txn = operation_lock.begin_write_immediate()?;
                txn.put_operation(&recovery).map_err(pristine_error)?;
                txn.compare_and_set_operation_heads(scope, &[head], &[recovery.id()])
                    .map_err(pristine_error)?;
                txn.commit()?;
                (head_operation, recovery, true)
            };

        self.replay_filesystem_recovery(operation_lock, &original, &recovery)?;
        self.finalize_operation_verified(operation_lock, recovery.id())?;
        Ok(RecoveryOutcome::Recovered {
            original: original.id(),
            recovery: recovery.id(),
            created,
        })
    }

    /// Observe the exact typed value of an effect supported by this repository engine.
    pub(super) fn observe_operation_effect(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<EffectValue, RepositoryError> {
        let target = self.resolve_recovery_target(working_copy, target)?;
        self.observe_recovery_target(&target)
    }

    /// Observe the exact typed value of a working-copy or shelf filesystem effect.
    pub(super) fn observe_filesystem_effect(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<EffectValue, RepositoryError> {
        let path = self
            .filesystem_effect_path(working_copy, target)?
            .ok_or_else(|| unsupported_effect_error(target))?;
        observe_path(&path, is_recursive_filesystem_target(target))
    }

    fn require_complete_single_head(
        &self,
        working_copy: WorkingCopyId,
        expected: OperationId,
    ) -> Result<(), RepositoryError> {
        let scope = OperationScope::WorkingCopy(working_copy);
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        match heads.as_slice() {
            [actual] if *actual == expected => {
                let receipts = txn
                    .get_effect_receipts(*actual)
                    .map_err(pristine_error)?;
                if has_operation_verified_receipt(&receipts) {
                    Ok(())
                } else {
                    Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "operation head {actual} is incomplete; recover it before preparing another operation"
                        ),
                    })
                }
            }
            [actual] => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation head changed while preparing switch: expected {expected}, found {actual}"
                ),
            }),
            many => Err(multiple_heads_error(scope, many)),
        }
    }

    fn load_operation(&self, operation_id: OperationId) -> Result<Operation, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        txn.get_operation(operation_id)
            .map_err(pristine_error)?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("operation not found: {operation_id}"),
            })
    }

    fn validate_operation_lock(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation: &Operation,
    ) -> Result<(), RepositoryError> {
        let locked = operation_lock.working_copy();
        match operation.payload().working_copy {
            Some(operation_working_copy) if operation_working_copy == locked => Ok(()),
            Some(operation_working_copy) => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation {} belongs to working copy {operation_working_copy}, but lock is for {locked}",
                    operation.id()
                ),
            }),
            None => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "repository-scoped operation {} cannot use a working-copy recovery lock",
                    operation.id()
                ),
            }),
        }
    }

    fn append_receipt_immediate(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation: &Operation,
        receipt: &EffectReceipt,
    ) -> Result<(), RepositoryError> {
        self.validate_operation_lock(operation_lock, operation)?;
        let mut txn = operation_lock.begin_write_immediate()?;
        txn.append_effect_receipt(receipt).map_err(pristine_error)?;
        txn.commit()
    }

    fn preflight_filesystem_leases(
        &self,
        working_copy: WorkingCopyId,
        operation: &Operation,
    ) -> Result<(), RepositoryError> {
        let mut simulated: Vec<(EffectTarget, EffectValue)> = Vec::new();
        for effect in &operation.payload().delta.effects {
            let observed = if let Some((_, value)) = simulated
                .iter()
                .find(|(target, _)| *target == effect.target)
            {
                Some(value.clone())
            } else if let Some(path) = self.filesystem_effect_path(working_copy, &effect.target)? {
                Some(observe_path(
                    &path,
                    is_recursive_filesystem_target(&effect.target),
                )?)
            } else if matches!(&effect.target, EffectTarget::WorkingCopy { .. }) {
                let target = self.resolve_recovery_target(working_copy, &effect.target)?;
                Some(self.observe_recovery_target(&target)?)
            } else {
                None
            };
            if let Some(observed) = observed {
                if observed != effect.expected_old {
                    return Err(lease_divergence_error(effect, &observed, None));
                }
                if let Some((_, value)) = simulated
                    .iter_mut()
                    .find(|(target, _)| *target == effect.target)
                {
                    *value = effect.expected_new.clone();
                } else {
                    simulated.push((effect.target.clone(), effect.expected_new.clone()));
                }
            }
        }
        Ok(())
    }

    fn snapshot_operation_filesystem(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation: &Operation,
    ) -> Result<PathBuf, RepositoryError> {
        self.validate_operation_lock(operation_lock, operation)?;
        let working_copy = operation_lock.working_copy();
        let root = self.operation_recovery_root(working_copy, operation.id());
        let complete = root.join(BACKUP_COMPLETE);
        if complete.is_file() {
            self.verify_backup(operation, &root)?;
            return Ok(root);
        }

        if root.exists() {
            let mut seen = Vec::new();
            for effect in &operation.payload().delta.effects {
                let Some(path) = self.filesystem_effect_path(working_copy, &effect.target)? else {
                    continue;
                };
                if seen.contains(&effect.target) {
                    continue;
                }
                seen.push(effect.target.clone());
                let observed = observe_path(&path, is_recursive_filesystem_target(&effect.target))?;
                if observed != effect.expected_old {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "incomplete recovery backup for operation {} cannot be rebuilt after effect {} moved away from expected-old",
                            operation.id(), effect.ordinal
                        ),
                    });
                }
            }
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(root.join(BACKUP_ENTRIES_DIR))?;

        let mut backed_up = Vec::new();
        for effect in &operation.payload().delta.effects {
            let Some(source) = self.filesystem_effect_path(working_copy, &effect.target)? else {
                continue;
            };
            if backed_up.contains(&effect.target) {
                continue;
            }
            backed_up.push(effect.target.clone());
            let observed_before =
                observe_path(&source, is_recursive_filesystem_target(&effect.target))?;
            if observed_before != effect.expected_old {
                return Err(lease_divergence_error(effect, &observed_before, None));
            }
            let entry = backup_entry_path(&root, effect.ordinal);
            fs::create_dir_all(&entry)?;
            match &effect.expected_old {
                EffectValue::Absent => {
                    write_new_synced(&entry.join(BACKUP_ABSENT), BACKUP_VERSION)?;
                }
                EffectValue::File(_) => {
                    let destination = entry.join(BACKUP_VALUE);
                    copy_entry(&source, &destination)?;
                    let backup_value =
                        observe_path(&destination, is_recursive_filesystem_target(&effect.target))?;
                    if backup_value != effect.expected_old {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "filesystem backup for operation {} effect {} does not match expected-old",
                                operation.id(), effect.ordinal
                            ),
                        });
                    }
                }
                value => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                        "filesystem effect {} expected-old must be Absent or File, found {value:?}",
                        effect.ordinal
                    ),
                    })
                }
            }
            let observed_after =
                observe_path(&source, is_recursive_filesystem_target(&effect.target))?;
            if observed_after != effect.expected_old {
                return Err(lease_divergence_error(effect, &observed_after, None));
            }
            sync_directory(&entry)?;
        }

        write_atomic_regular(&complete, BACKUP_VERSION, 0o600)?;
        sync_directory(&root)?;
        if let Some(parent) = root.parent() {
            sync_directory(parent)?;
        }
        self.verify_backup(operation, &root)?;
        Ok(root)
    }

    fn verify_backup(&self, operation: &Operation, root: &Path) -> Result<(), RepositoryError> {
        let marker = fs::read(root.join(BACKUP_COMPLETE))?;
        if marker != BACKUP_VERSION {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation recovery backup {} has an invalid complete marker",
                    root.display()
                ),
            });
        }
        let mut verified_targets = Vec::new();
        for effect in &operation.payload().delta.effects {
            if !is_filesystem_effect(&effect.target) || verified_targets.contains(&effect.target) {
                continue;
            }
            verified_targets.push(effect.target.clone());
            let entry = backup_entry_path(root, effect.ordinal);
            match &effect.expected_old {
                EffectValue::Absent => {
                    if fs::read(entry.join(BACKUP_ABSENT))? != BACKUP_VERSION {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "operation {} effect {} has an invalid absent backup",
                                operation.id(),
                                effect.ordinal
                            ),
                        });
                    }
                }
                EffectValue::File(_) => {
                    let observed = observe_path(
                        &entry.join(BACKUP_VALUE),
                        is_recursive_filesystem_target(&effect.target),
                    )?;
                    if observed != effect.expected_old {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "operation {} effect {} backup no longer matches expected-old",
                                operation.id(),
                                effect.ordinal
                            ),
                        });
                    }
                }
                value => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                        "filesystem effect {} expected-old must be Absent or File, found {value:?}",
                        effect.ordinal
                    ),
                    })
                }
            }
        }
        Ok(())
    }

    fn replay_filesystem_recovery(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        original: &Operation,
        recovery: &Operation,
    ) -> Result<(), RepositoryError> {
        self.validate_operation_lock(operation_lock, recovery)?;
        let working_copy = operation_lock.working_copy();
        let backup_root = self.operation_recovery_root(working_copy, original.id());
        let receipts = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_effect_receipts(recovery.id())
                .map_err(pristine_error)?
        };
        let completed: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::Applied
                | EffectReceiptKind::RolledBack
                | EffectReceiptKind::Recovered => receipt.payload().effect_ordinal,
                EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
            })
            .collect();

        let mut classified = Vec::with_capacity(recovery.payload().delta.effects.len());
        let mut simulated: Vec<(EffectTarget, EffectValue)> = Vec::new();
        for effect in &recovery.payload().delta.effects {
            let target = self.resolve_recovery_target(working_copy, &effect.target)?;
            let observed = if let Some((_, value)) = simulated
                .iter()
                .find(|(candidate, _)| *candidate == effect.target)
            {
                value.clone()
            } else {
                self.observe_recovery_target(&target)?
            };
            let classification =
                classify_effect_lease(&observed, &effect.expected_old, &effect.expected_new);
            if classification == LeaseClassification::Diverged {
                let receipt = deterministic_effect_receipt(
                    recovery,
                    Some(effect.ordinal),
                    EffectReceiptKind::LeaseRejected,
                    Some(observed.clone()),
                    None,
                )?;
                self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                return Err(lease_divergence_error(effect, &observed, None));
            }
            if let Some((_, value)) = simulated
                .iter_mut()
                .find(|(candidate, _)| *candidate == effect.target)
            {
                *value = effect.expected_new.clone();
            } else {
                simulated.push((effect.target.clone(), effect.expected_new.clone()));
            }
            classified.push((effect, target));
        }

        for (effect, target) in classified {
            let observed = self.observe_recovery_target(&target)?;
            let classification =
                classify_effect_lease(&observed, &effect.expected_old, &effect.expected_new);
            match classification {
                LeaseClassification::Apply => {
                    let mut shelf_txn = if matches!(
                        effect.target,
                        EffectTarget::ShelfPath { .. } | EffectTarget::WorkspacePath { .. }
                    ) {
                        Some(operation_lock.begin_write_immediate()?.try_lock_shelf()?)
                    } else {
                        None
                    };
                    self.restore_original_effect(
                        operation_lock,
                        original,
                        effect,
                        &target,
                        &backup_root,
                    )?;
                    let restored = self.observe_recovery_target(&target)?;
                    if restored != effect.expected_new {
                        let receipt = deterministic_effect_receipt(
                            recovery,
                            Some(effect.ordinal),
                            EffectReceiptKind::LeaseRejected,
                            Some(observed.clone()),
                            Some(restored.clone()),
                        )?;
                        if let Some(mut txn) = shelf_txn {
                            txn.append_effect_receipt(&receipt)
                                .map_err(pristine_error)?;
                            txn.commit()?;
                        } else {
                            self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                        }
                        return Err(lease_divergence_error(effect, &observed, Some(&restored)));
                    }
                    let receipt = deterministic_effect_receipt(
                        recovery,
                        Some(effect.ordinal),
                        EffectReceiptKind::RolledBack,
                        Some(observed),
                        Some(restored),
                    )?;
                    if let Some(mut txn) = shelf_txn.take() {
                        txn.append_effect_receipt(&receipt)
                            .map_err(pristine_error)?;
                        txn.commit()?;
                    } else {
                        self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                    }
                }
                LeaseClassification::AlreadyApplied => {
                    if let (
                        ResolvedRecoveryTarget::WorkingCopy(id),
                        EffectValue::WorkingCopy(state),
                    ) = (&target, &effect.expected_new)
                    {
                        if *id != state.id {
                            return Err(RepositoryError::InvalidOperation {
                                message: format!(
                                    "working-copy recovery target {id} disagrees with state {}",
                                    state.id
                                ),
                            });
                        }
                        self.apply_working_copy_state_locked(operation_lock, state)?;
                    }
                    if !completed.contains(&effect.ordinal) {
                        let receipt = deterministic_effect_receipt(
                            recovery,
                            Some(effect.ordinal),
                            EffectReceiptKind::Recovered,
                            Some(observed.clone()),
                            Some(observed),
                        )?;
                        self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                    }
                }
                LeaseClassification::Diverged => {
                    let receipt = deterministic_effect_receipt(
                        recovery,
                        Some(effect.ordinal),
                        EffectReceiptKind::LeaseRejected,
                        Some(observed.clone()),
                        None,
                    )?;
                    self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                    return Err(lease_divergence_error(effect, &observed, None));
                }
            }
        }
        Ok(())
    }

    fn restore_original_effect(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        original: &Operation,
        recovery_effect: &EffectPlan,
        target: &ResolvedRecoveryTarget,
        backup_root: &Path,
    ) -> Result<(), RepositoryError> {
        let matches: Vec<&EffectPlan> = original
            .payload()
            .delta
            .effects
            .iter()
            .filter(|effect| {
                recovery_effect.target == effect.target
                    && recovery_effect.expected_new == effect.expected_old
                    && recovery_effect.expected_old == effect.expected_new
            })
            .collect();
        let original_effect = match matches.as_slice() {
            [effect] => *effect,
            _ => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                    "recovery effect {} does not identify exactly one original inverse transition",
                    recovery_effect.ordinal
                ),
                })
            }
        };

        match (&original_effect.expected_old, target) {
            (EffectValue::Absent, ResolvedRecoveryTarget::Filesystem { path, .. }) => {
                let tombstone_root =
                    if matches!(original_effect.target, EffectTarget::ShelfPath { .. }) {
                        backup_root.to_path_buf()
                    } else {
                        self.working_copy_effect_recovery_root(
                            operation_lock.working_copy(),
                            original.id(),
                        )
                    };
                remove_entry_for_recovery(
                    path,
                    &tombstone_root
                        .join("rolled-back-effects")
                        .join(format!("{:010}", original_effect.ordinal)),
                )
            }
            (EffectValue::File(state), ResolvedRecoveryTarget::Filesystem { path, .. }) => {
                self.verify_backup(original, backup_root)?;
                let source =
                    backup_entry_path(backup_root, original_effect.ordinal).join(BACKUP_VALUE);
                match state.kind {
                    FileKind::Regular => write_atomic_regular_from_file(path, &source, state.mode),
                    FileKind::Directory => replace_directory(path, &source, state.mode),
                    FileKind::Symlink => replace_symlink(path, &source),
                    FileKind::Gitlink => Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "filesystem recovery does not synthesize gitlink effect {}",
                            recovery_effect.ordinal
                        ),
                    }),
                }
            }
            (EffectValue::WorkingCopy(state), ResolvedRecoveryTarget::WorkingCopy(id))
                if state.id == *id =>
            {
                self.apply_working_copy_state_locked(operation_lock, state)
            }
            (value, target) => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "recovery cannot restore value {value:?} through resolved target {target:?}"
                ),
            }),
        }
    }

    fn inverse_recovery_operation(
        &self,
        original: &Operation,
        working_copy: WorkingCopyId,
    ) -> Result<Operation, RepositoryError> {
        let receipts = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_effect_receipts(original.id())
                .map_err(pristine_error)?
        };
        let completed: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::Applied
                | EffectReceiptKind::Recovered
                | EffectReceiptKind::RolledBack => receipt.payload().effect_ordinal,
                EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
            })
            .collect();

        let original_effects = &original.payload().delta.effects;
        let mut selected = BTreeSet::new();
        let mut visited_targets = Vec::new();
        for first in original_effects {
            if visited_targets.contains(&first.target) {
                continue;
            }
            visited_targets.push(first.target.clone());
            let chain: Vec<&EffectPlan> = original_effects
                .iter()
                .filter(|effect| effect.target == first.target)
                .collect();
            let target = self.resolve_recovery_target(working_copy, &first.target)?;
            let observed = self.observe_recovery_target(&target)?;
            let mut states = Vec::with_capacity(chain.len() + 1);
            states.push(chain[0].expected_old.clone());
            states.extend(chain.iter().map(|effect| effect.expected_new.clone()));
            let minimum_reached = chain
                .iter()
                .enumerate()
                .filter(|(_, effect)| completed.contains(&effect.ordinal))
                .map(|(index, _)| index + 1)
                .max()
                .unwrap_or(0);
            let candidates: Vec<usize> = states
                .iter()
                .enumerate()
                .filter_map(|(index, state)| {
                    (index >= minimum_reached && state == &observed).then_some(index)
                })
                .collect();
            let reached = match candidates.as_slice() {
                [reached] => *reached,
                // The executor commits each stage's receipt before it may run
                // the next transition for the same target. With no successful
                // receipt, a cyclic endpoint equal to the initial value is
                // therefore unambiguously the not-started state.
                [first, ..] if minimum_reached == 0 && *first == 0 => 0,
                [] => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "operation {} target {:?} diverged before recovery: observed {:?}, expected one of {:?}",
                            original.id(), first.target, observed, states
                        ),
                    })
                }
                _ => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "operation {} target {:?} has an ambiguous cyclic lease chain at observed value {:?}",
                            original.id(), first.target, observed
                        ),
                    })
                }
            };
            for effect in chain.into_iter().take(reached) {
                selected.insert(effect.ordinal);
            }
        }

        let effects = original_effects
            .iter()
            .rev()
            .filter(|effect| selected.contains(&effect.ordinal))
            .enumerate()
            .map(|(ordinal, effect)| {
                Ok(EffectPlan {
                    ordinal: u32::try_from(ordinal).map_err(|_| {
                        RepositoryError::InvalidOperation {
                            message: "too many effects to construct inverse recovery operation"
                                .to_string(),
                        }
                    })?,
                    target: effect.target.clone(),
                    expected_old: effect.expected_new.clone(),
                    expected_new: effect.expected_old.clone(),
                })
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;
        Operation::new(OperationPayload {
            parents: vec![original.id()],
            kind: OperationKind::Recover,
            working_copy: original.payload().working_copy,
            before: original.payload().delta.after.clone(),
            delta: RepoStateDelta {
                after: original.payload().before.clone(),
                effects,
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: RECOVERY_ACTOR.to_string(),
            },
            timestamp_ms: original.payload().timestamp_ms,
            lossy: Vec::new(),
        })
        .map_err(codec_error)
    }

    fn resolve_recovery_target(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<ResolvedRecoveryTarget, RepositoryError> {
        if let Some(path) = self.filesystem_effect_path(working_copy, target)? {
            return Ok(ResolvedRecoveryTarget::Filesystem {
                path,
                recursive_directory: is_recursive_filesystem_target(target),
            });
        }
        match target {
            EffectTarget::WorkingCopy {
                working_copy: target_working_copy,
            } if *target_working_copy == working_copy => {
                Ok(ResolvedRecoveryTarget::WorkingCopy(working_copy))
            }
            EffectTarget::WorkingCopy {
                working_copy: target_working_copy,
            } => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "working-copy effect belongs to {target_working_copy}, operation lock is for {working_copy}"
                ),
            }),
            _ => Err(unsupported_effect_error(target)),
        }
    }

    fn observe_recovery_target(
        &self,
        target: &ResolvedRecoveryTarget,
    ) -> Result<EffectValue, RepositoryError> {
        match target {
            ResolvedRecoveryTarget::Filesystem {
                path,
                recursive_directory,
            } => observe_path(path, *recursive_directory),
            ResolvedRecoveryTarget::WorkingCopy(id) => {
                let txn = self.pristine.read_txn().map_err(pristine_error)?;
                let record = txn
                    .get_working_copy(*id)
                    .map_err(pristine_error)?
                    .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: *id })?;
                Ok(EffectValue::WorkingCopy(working_copy_state_ref(record)))
            }
        }
    }

    fn filesystem_effect_path(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<Option<PathBuf>, RepositoryError> {
        match target {
            EffectTarget::FilesystemPath { path } => {
                let relative = validate_relative_path(path, false)?;
                resolve_without_symlink_parents(&self.root, &relative).map(Some)
            }
            EffectTarget::WorkspacePath {
                working_copy: target_working_copy,
                path,
            } => {
                if *target_working_copy != working_copy {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "workspace effect belongs to working copy {target_working_copy}, operation lock is for {working_copy}"
                        ),
                    });
                }
                let relative = validate_relative_path(path, false)?;
                resolve_without_symlink_parents(&self.root, &relative).map(Some)
            }
            EffectTarget::ShelfPath {
                working_copy: target_working_copy,
                view,
                path,
            } => {
                if *target_working_copy != working_copy {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "shelf effect belongs to working copy {target_working_copy}, operation lock is for {working_copy}"
                        ),
                    });
                }
                let view = validate_relative_path(view, true)?;
                let path = validate_relative_path(path, true)?;
                let base = self
                    .dot_dir
                    .join("working-copies")
                    .join(working_copy.to_string())
                    .join("workspaces");
                let view_root = resolve_without_symlink_parents(&base, &view)?;
                resolve_without_symlink_parents(&view_root, &path).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn working_copy_effect_recovery_root(
        &self,
        _working_copy: WorkingCopyId,
        operation: OperationId,
    ) -> PathBuf {
        self.working_copy_dot_dir()
            .join(RECOVERY_DIR)
            .join(operation.to_string())
    }

    fn operation_recovery_root(
        &self,
        working_copy: WorkingCopyId,
        original: OperationId,
    ) -> PathBuf {
        self.dot_dir
            .join("working-copies")
            .join(working_copy.to_string())
            .join(RECOVERY_DIR)
            .join(original.to_string())
    }
}

fn sole_recovery_parent(recovery: &Operation) -> Result<OperationId, RepositoryError> {
    match recovery.payload().parents.as_slice() {
        [parent] => Ok(*parent),
        parents => Err(RepositoryError::InvalidOperation {
            message: format!(
                "Recover operation {} must have exactly one parent for CB-1B, found {}",
                recovery.id(),
                parents.len()
            ),
        }),
    }
}

fn validate_effect_target_chains(operation: &Operation) -> Result<(), RepositoryError> {
    let mut states: Vec<(&EffectTarget, &EffectValue)> = Vec::new();
    for effect in &operation.payload().delta.effects {
        if let Some((_, previous_new)) = states
            .iter_mut()
            .find(|(target, _)| **target == effect.target)
        {
            if **previous_new != effect.expected_old {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "switch operation {} repeats effect target at ordinal {} without chaining expected-old from the previous expected-new value",
                        operation.id(), effect.ordinal
                    ),
                });
            }
            *previous_new = &effect.expected_new;
        } else {
            states.push((&effect.target, &effect.expected_new));
        }
    }
    Ok(())
}

fn effect_at(operation: &Operation, ordinal: u32) -> Result<&EffectPlan, RepositoryError> {
    operation
        .payload()
        .delta
        .effects
        .get(ordinal as usize)
        .filter(|effect| effect.ordinal == ordinal)
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "operation {} has no effect ordinal {ordinal}",
                operation.id()
            ),
        })
}

pub(super) fn working_copy_state_ref(record: WorkingCopyRecord) -> WorkingCopyStateRef {
    WorkingCopyStateRef {
        id: record.id,
        location_fingerprint: record.location_fingerprint,
        desired_view: record.desired_view,
        desired_state: record.desired_state,
        materialized_state: record.materialized_state,
        materialized_manifest: record.materialized_manifest,
    }
}

fn validate_state_working_copy(
    state: &RepoStateRef,
    working_copy: WorkingCopyId,
) -> Result<(), RepositoryError> {
    match &state.working_copy {
        Some(value) if value.id == working_copy => Ok(()),
        Some(value) => Err(RepositoryError::InvalidOperation {
            message: format!(
                "operation state belongs to working copy {}, lock is for {working_copy}",
                value.id
            ),
        }),
        None => Err(RepositoryError::InvalidOperation {
            message: format!(
                "working-copy operation for {working_copy} requires working-copy state"
            ),
        }),
    }
}

fn multiple_heads_error(scope: OperationScope, heads: &[OperationId]) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!(
            "{scope} has {} operation heads; CB-1B refuses consolidation until CB-1C: {}",
            heads.len(),
            heads
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn lease_divergence_error(
    effect: &EffectPlan,
    observed_before: &EffectValue,
    observed_after: Option<&EffectValue>,
) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: match observed_after {
            Some(after) => format!(
                "effect {} diverged: expected old {:?} and new {:?}, observed before {:?} and after {:?}",
                effect.ordinal,
                effect.expected_old,
                effect.expected_new,
                observed_before,
                after
            ),
            None => format!(
                "effect {} diverged: expected old {:?} or new {:?}, observed {:?}",
                effect.ordinal, effect.expected_old, effect.expected_new, observed_before
            ),
        },
    }
}

fn unsupported_effect_error(target: &EffectTarget) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!(
            "CB-1B repository recovery cannot execute effect target {target:?}; parent integration must provide its lease-safe executor"
        ),
    }
}

fn codec_error(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!("invalid operation journal object: {error}"),
    }
}

fn pristine_error(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

fn is_filesystem_effect(target: &EffectTarget) -> bool {
    matches!(
        target,
        EffectTarget::FilesystemPath { .. }
            | EffectTarget::WorkspacePath { .. }
            | EffectTarget::ShelfPath { .. }
    )
}

fn is_recursive_filesystem_target(target: &EffectTarget) -> bool {
    matches!(
        target,
        EffectTarget::WorkspacePath { .. } | EffectTarget::ShelfPath { .. }
    )
}

fn backup_entry_path(root: &Path, ordinal: u32) -> PathBuf {
    root.join(BACKUP_ENTRIES_DIR).join(format!("{ordinal:010}"))
}

fn validate_relative_path(path: &str, allow_vcs_names: bool) -> Result<PathBuf, RepositoryError> {
    if path.is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: "operation effect path cannot be empty".to_string(),
        });
    }
    let candidate = Path::new(path);
    let mut clean = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(value) => clean.push(value),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("unsafe operation effect path '{path}'"),
                })
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: format!("unsafe operation effect path '{path}'"),
        });
    }
    if !allow_vcs_names {
        let first = clean
            .components()
            .next()
            .and_then(|component| match component {
                Component::Normal(value) => Some(value),
                _ => None,
            });
        if first == Some(OsStr::new(".atomic")) || first == Some(OsStr::new(".git")) {
            return Err(RepositoryError::InvalidOperation {
                message: format!("operation filesystem effect cannot target '{path}'"),
            });
        }
    }
    Ok(clean)
}

fn resolve_without_symlink_parents(
    base: &Path,
    relative: &Path,
) -> Result<PathBuf, RepositoryError> {
    let mut resolved = base.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(value) = component else {
            return Err(RepositoryError::InvalidOperation {
                message: format!("unsafe operation path '{}'", relative.display()),
            });
        };
        resolved.push(value);
        if index + 1 == components.len() {
            continue;
        }
        match fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation path '{}' traverses symlink parent '{}'",
                        relative.display(),
                        resolved.display()
                    ),
                })
            }
            Ok(_) => {}
            Err(error) if is_absent_path_error(&error) => {}
            Err(error) => return Err(RepositoryError::Io(error)),
        }
    }
    Ok(resolved)
}

fn is_absent_path_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn observe_path(path: &Path, recursive_directory: bool) -> Result<EffectValue, RepositoryError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if is_absent_path_error(&error) => {
            return Ok(EffectValue::Absent);
        }
        Err(error) => return Err(RepositoryError::Io(error)),
    };
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_file() {
        FileKind::Regular
    } else if file_type.is_dir() {
        FileKind::Directory
    } else {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "operation cannot classify special filesystem entry '{}'",
                path.display()
            ),
        });
    };
    let content = match kind {
        FileKind::Regular => Hash::of(&fs::read(path)?),
        FileKind::Symlink => Hash::of(&os_str_bytes(fs::read_link(path)?.as_os_str())),
        FileKind::Directory if recursive_directory => directory_content_hash(path)?,
        FileKind::Directory => Hash::of(b"atomic:filesystem-directory-entry:v1\0"),
        FileKind::Gitlink => unreachable!(),
    };
    Ok(EffectValue::File(FileState {
        kind,
        mode: metadata_mode(&metadata),
        content,
    }))
}

fn directory_content_hash(path: &Path) -> Result<Hash, RepositoryError> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| {
        os_str_bytes(left.file_name().as_os_str()).cmp(&os_str_bytes(right.file_name().as_os_str()))
    });
    let mut canonical = b"atomic:operation-directory:v1\0".to_vec();
    for entry in entries {
        let name = os_str_bytes(entry.file_name().as_os_str());
        let value = observe_path(&entry.path(), true)?;
        let EffectValue::File(state) = value else {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "directory entry '{}' disappeared while hashing",
                    entry.path().display()
                ),
            });
        };
        canonical.extend_from_slice(&(name.len() as u64).to_le_bytes());
        canonical.extend_from_slice(&name);
        canonical.push(match state.kind {
            FileKind::Regular => 1,
            FileKind::Directory => 2,
            FileKind::Symlink => 3,
            FileKind::Gitlink => 4,
        });
        canonical.extend_from_slice(&state.mode.to_le_bytes());
        canonical.extend_from_slice(state.content.as_bytes());
    }
    Ok(Hash::of(&canonical))
}

fn copy_entry(source: &Path, destination: &Path) -> Result<(), RepositoryError> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        create_symlink(&fs::read_link(source)?, destination, source)?;
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut input = File::open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        std::io::copy(&mut input, &mut output)?;
        set_mode(destination, metadata_mode(&metadata))?;
        output.sync_all()?;
    } else if metadata.is_dir() {
        fs::create_dir(destination)?;
        let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by(|left, right| {
            os_str_bytes(left.file_name().as_os_str())
                .cmp(&os_str_bytes(right.file_name().as_os_str()))
        });
        for entry in entries {
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
        set_mode(destination, metadata_mode(&metadata))?;
        sync_directory(destination)?;
    } else {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "operation cannot back up special filesystem entry '{}'",
                source.display()
            ),
        });
    }
    Ok(())
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), RepositoryError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_atomic_regular(path: &Path, bytes: &[u8], mode: u32) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "cannot atomically replace path without parent: {}",
                path.display()
            ),
        })?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file_mut().write_all(bytes)?;
    set_mode(temporary.path(), mode)?;
    temporary.as_file_mut().sync_all()?;
    if matches!(fs::symlink_metadata(path), Ok(metadata) if metadata.is_dir()) {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "refusing to replace directory '{}' with a regular file",
                path.display()
            ),
        });
    }
    temporary
        .persist(path)
        .map_err(|error| RepositoryError::Io(error.error))?;
    sync_directory(parent)
}

fn write_atomic_regular_from_file(
    path: &Path,
    source: &Path,
    mode: u32,
) -> Result<(), RepositoryError> {
    let mut input = File::open(source)?;
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    write_atomic_regular(path, &bytes, mode)
}

fn replace_directory(path: &Path, source: &Path, mode: u32) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "cannot replace directory without parent: {}",
                path.display()
            ),
        })?;
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(path).is_ok() {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "directory recovery target must be absent before replacement: {}",
                path.display()
            ),
        });
    }
    let staging = tempfile::Builder::new()
        .prefix(".atomic-recover-dir-")
        .tempdir_in(parent)?;
    let prepared = staging.path().join(BACKUP_VALUE);
    copy_entry(source, &prepared)?;
    set_mode(&prepared, mode)?;
    fs::rename(&prepared, path)?;
    sync_directory(parent)
}

fn replace_symlink(path: &Path, source: &Path) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("cannot replace symlink without parent: {}", path.display()),
        })?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".atomic-recover-link-")
        .tempdir_in(parent)?;
    let prepared = staging.path().join(BACKUP_VALUE);
    let target = fs::read_link(source)?;
    create_symlink(&target, &prepared, source)?;
    fs::rename(&prepared, path)?;
    sync_directory(parent)
}

fn create_directory_effect_path(
    path: &Path,
    mode: u32,
    staging: &Path,
) -> Result<(), RepositoryError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "refusing to replace existing entry '{}' with a directory",
                    path.display()
                ),
            })
        }
        Err(error) => return Err(RepositoryError::Io(error)),
    }
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("directory effect has no parent: {}", path.display()),
        })?;
    if !parent.is_dir() {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "directory effect parent '{}' is not materialized",
                parent.display()
            ),
        });
    }
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    let staging_parent = staging
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "directory staging path has no parent: {}",
                staging.display()
            ),
        })?;
    fs::create_dir_all(staging_parent)?;
    fs::create_dir(staging)?;
    set_mode(staging, mode)?;
    sync_directory(staging)?;
    sync_directory(staging_parent)?;
    fs::rename(staging, path)?;
    sync_directory(parent)
}

fn remove_filesystem_effect_path(path: &Path) -> Result<(), RepositoryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir(path)?
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RepositoryError::Io(error)),
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn remove_entry_for_recovery(path: &Path, tombstone: &Path) -> Result<(), RepositoryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let parent = tombstone
                .parent()
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("recovery tombstone has no parent: {}", tombstone.display()),
                })?;
            fs::create_dir_all(parent)?;
            if tombstone.exists() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "recovery tombstone already exists while target is present: {}",
                        tombstone.display()
                    ),
                });
            }
            fs::rename(path, tombstone)?;
            if let Some(source_parent) = path.parent() {
                sync_directory(source_parent)?;
            }
            sync_directory(parent)?;
        }
        Ok(_) => {
            fs::remove_file(path)?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RepositoryError::Io(error)),
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(
    target: &Path,
    destination: &Path,
    _source: &Path,
) -> Result<(), RepositoryError> {
    std::os::unix::fs::symlink(target, destination).map_err(RepositoryError::Io)
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, source: &Path) -> Result<(), RepositoryError> {
    let target_is_dir = fs::metadata(source)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination).map_err(RepositoryError::Io)
    } else {
        std::os::windows::fs::symlink_file(target, destination).map_err(RepositoryError::Io)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(
    _target: &Path,
    destination: &Path,
    _source: &Path,
) -> Result<(), RepositoryError> {
    Err(RepositoryError::InvalidOperation {
        message: format!(
            "symbolic-link recovery is unsupported on this platform: {}",
            destination.display()
        ),
    })
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), RepositoryError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(RepositoryError::Io)
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> Result<(), RepositoryError> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, permissions).map_err(RepositoryError::Io)
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

fn sync_directory(path: &Path) -> Result<(), RepositoryError> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

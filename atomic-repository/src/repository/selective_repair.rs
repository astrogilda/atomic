//! Durable planning state for selective repair.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use atomic_core::types::{Hash, Merkle};
use serde::{Deserialize, Serialize};

use super::{Repository, RepositoryError};

/// Current on-disk selective-repair plan format.
pub const SELECTIVE_REPAIR_PLAN_VERSION: u32 = 1;
const SELECTIVE_REPAIR_PLAN_FILE: &str = "selective-repair.pending.json";

/// Progress through the crash-recoverable selective-repair workflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectiveRepairPhase {
    Planned,
    Evaluated,
    BuildingReplacements,
    ReadyToPublish,
    ViewPublished,
    Completed,
    Aborted,
}

/// A change and its stable position in a view.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrderedChange {
    pub change: Hash,
    pub order: u64,
}

/// A later change that structurally depends on changes being replaced.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StructuralBlocker {
    pub change: Hash,
    /// Changes in the repair closure referenced by this blocker.
    pub references: Vec<Hash>,
}

/// Counterfactual evidence refining the conservative candidate closure.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectiveRepairEvaluation {
    /// Candidates whose contribution disappears or fails when the target is absent.
    pub confirmed_blockers: Vec<Hash>,
    /// Candidates that still contribute with the target absent.
    pub context_only: Vec<Hash>,
    /// First affected path proving a context-only candidate survives.
    pub surviving_paths: BTreeMap<Hash, String>,
}

/// The replacement generated for one original change.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Replacement {
    pub original: Hash,
    pub replacement: Hash,
    /// Signed executable patch relink carrying position aliases and removals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_relink: Option<Hash>,
    /// Signed provenance relink objects connecting original evidence to this replacement.
    #[serde(default)]
    pub provenance_relinks: Vec<Hash>,
}

/// Wall-clock timestamps are strings so the durable format does not impose a
/// timestamp library or discard the caller's precision/offset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectiveRepairTimestamps {
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// Versioned, durable description of a selective repair. This type only
/// records and validates a plan; it never mutates repository graph state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectiveRepairPlan {
    pub version: u32,
    pub plan_id: String,
    pub generation: u64,
    pub phase: SelectiveRepairPhase,
    pub view: String,
    pub original_state: Merkle,
    pub original_order: Vec<OrderedChange>,
    pub target: Hash,
    pub blockers: Vec<StructuralBlocker>,
    pub commuting: Vec<Hash>,
    /// Replacement records keyed by original change.
    pub replacements: BTreeMap<Hash, Replacement>,
    pub replacement_order: Vec<OrderedChange>,
    /// Exact provenance objects discovered for each original repair-closure change.
    /// An empty vector means the lookup completed and found no provenance.
    pub original_provenance: BTreeMap<Hash, Vec<Hash>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<SelectiveRepairEvaluation>,
    pub timestamps: SelectiveRepairTimestamps,
}

impl SelectiveRepairPlan {
    /// Validate the durable plan's internal consistency.
    pub fn validate(&self) -> Result<(), RepositoryError> {
        let invalid = |message: &str| RepositoryError::InvalidOperation {
            message: format!("invalid selective repair plan: {message}"),
        };

        if self.version != SELECTIVE_REPAIR_PLAN_VERSION {
            return Err(invalid("unsupported version"));
        }
        if self.plan_id.trim().is_empty() || self.view.trim().is_empty() {
            return Err(invalid("plan_id and view must be nonempty"));
        }
        if self.timestamps.created_at.trim().is_empty()
            || self.timestamps.updated_at.trim().is_empty()
        {
            return Err(invalid("created_at and updated_at must be nonempty"));
        }

        validate_order(&self.original_order, "original_order")?;
        if self
            .original_order
            .iter()
            .filter(|entry| entry.change == self.target)
            .count()
            != 1
        {
            return Err(invalid("target must occur exactly once in original_order"));
        }
        let target_order = self
            .original_order
            .iter()
            .find(|entry| entry.change == self.target)
            .expect("target count checked")
            .order;
        let later: BTreeSet<_> = self
            .original_order
            .iter()
            .filter(|entry| entry.order > target_order)
            .map(|entry| entry.change)
            .collect();

        let blocker_changes: BTreeSet<_> = self.blockers.iter().map(|b| b.change).collect();
        if blocker_changes.len() != self.blockers.len() {
            return Err(invalid("blocker changes must be unique"));
        }
        if self
            .blockers
            .iter()
            .any(|blocker| blocker.references.is_empty())
        {
            return Err(invalid("every blocker must have at least one reference"));
        }
        let commuting: BTreeSet<_> = self.commuting.iter().copied().collect();
        if commuting.len() != self.commuting.len() || !blocker_changes.is_disjoint(&commuting) {
            return Err(invalid(
                "blockers and commuting changes must be unique and disjoint",
            ));
        }
        let accounted: BTreeSet<_> = blocker_changes.union(&commuting).copied().collect();
        if accounted != later {
            return Err(invalid(
                "blockers and commuting changes must account for every later change",
            ));
        }

        let mut closure = blocker_changes;
        closure.insert(self.target);
        for blocker in &self.blockers {
            if blocker
                .references
                .iter()
                .any(|reference| !closure.contains(reference))
            {
                return Err(invalid(
                    "blocker references must remain within the repair closure",
                ));
            }
        }
        let replacement_keys: BTreeSet<_> = self.replacements.keys().copied().collect();
        if !replacement_keys.is_subset(&closure)
            || self
                .replacements
                .iter()
                .any(|(key, replacement)| *key != replacement.original)
        {
            return Err(invalid(
                "replacement keys must identify originals in the repair closure",
            ));
        }

        let replacement_order = validate_order(&self.replacement_order, "replacement_order")?;
        let replacement_hashes: BTreeSet<_> = self
            .replacements
            .values()
            .map(|replacement| replacement.replacement)
            .collect();
        if replacement_hashes.len() != self.replacements.len()
            || replacement_order != replacement_hashes
        {
            return Err(invalid(
                "replacement_order must contain every replacement exactly once",
            ));
        }
        if let Some(evaluation) = &self.evaluation {
            let confirmed: BTreeSet<_> = evaluation.confirmed_blockers.iter().copied().collect();
            let context_only: BTreeSet<_> = evaluation.context_only.iter().copied().collect();
            let candidates: BTreeSet<_> = closure
                .iter()
                .copied()
                .filter(|hash| *hash != self.target)
                .collect();
            if !confirmed.is_disjoint(&context_only)
                || confirmed
                    .union(&context_only)
                    .copied()
                    .collect::<BTreeSet<_>>()
                    != candidates
            {
                return Err(invalid("evaluation must partition every candidate blocker"));
            }
            if evaluation
                .surviving_paths
                .keys()
                .any(|hash| !context_only.contains(hash))
            {
                return Err(invalid(
                    "surviving path evidence must belong to context-only candidates",
                ));
            }
        }
        if matches!(self.phase, SelectiveRepairPhase::Evaluated) && self.evaluation.is_none() {
            return Err(invalid("evaluated plans require evaluation evidence"));
        }

        if self
            .original_provenance
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
            != closure
        {
            return Err(invalid(
                "original_provenance must inventory every repair-closure change",
            ));
        }
        if self.replacements.values().any(|replacement| {
            (replacement.patch_relink.is_none()
                || (!self.original_provenance[&replacement.original].is_empty()
                    && replacement.provenance_relinks.is_empty()))
                && matches!(
                    self.phase,
                    SelectiveRepairPhase::ReadyToPublish
                        | SelectiveRepairPhase::ViewPublished
                        | SelectiveRepairPhase::Completed
                )
        }) {
            return Err(invalid(
                "ready and published replacements require provenance relinks",
            ));
        }
        if matches!(
            self.phase,
            SelectiveRepairPhase::ReadyToPublish
                | SelectiveRepairPhase::ViewPublished
                | SelectiveRepairPhase::Completed
        ) && replacement_keys != closure
        {
            return Err(invalid(
                "ready and published plans require all replacements",
            ));
        }
        Ok(())
    }
}

fn validate_order(
    entries: &[OrderedChange],
    field: &str,
) -> Result<BTreeSet<Hash>, RepositoryError> {
    let mut previous = None;
    let mut changes = BTreeSet::new();
    for entry in entries {
        if previous.is_some_and(|order| entry.order <= order) || !changes.insert(entry.change) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "invalid selective repair plan: {field} must have monotonic unique order and unique changes"
                ),
            });
        }
        previous = Some(entry.order);
    }
    Ok(changes)
}

impl Repository {
    /// Run a read-only counterfactual second pass over candidate blockers.
    ///
    /// Each candidate is compared on its affected paths with the target absent,
    /// first with the candidate visible and then with it absent. An observable
    /// difference proves the candidate still contributes independently; a
    /// traversal failure or complete disappearance remains conservatively blocked.
    pub fn evaluate_selective_repair_plan(
        &self,
        updated_at: String,
    ) -> Result<SelectiveRepairPlan, RepositoryError> {
        let mut plan = self.load_selective_repair_plan()?.ok_or_else(|| {
            RepositoryError::InvalidOperation {
                message: "no active selective repair plan".to_string(),
            }
        })?;
        let view = self.get_view_info(&plan.view)?;
        if view.state != plan.original_state {
            return Err(RepositoryError::InvalidOperation {
                message: "selective repair view diverged after planning".to_string(),
            });
        }
        if self.current_view() != plan.view {
            return Err(RepositoryError::InvalidOperation {
                message: format!("switch to '{}' before evaluating isolation", plan.view),
            });
        }

        let mut evaluation = SelectiveRepairEvaluation::default();
        for blocker in &plan.blockers {
            let change = self.load_change(&blocker.change)?;
            let paths: BTreeSet<String> = change
                .hunks()
                .iter()
                .filter_map(|op| op.path().map(str::to_owned))
                .collect();
            let mut surviving_path = None;
            for path in paths {
                let with_candidate = self.get_file_content_after_change_excluding(
                    &path,
                    &blocker.change,
                    &[plan.target],
                );
                let without_candidate = self.get_file_content_after_change_excluding(
                    &path,
                    &blocker.change,
                    &[plan.target, blocker.change],
                );
                match (with_candidate, without_candidate) {
                    (Ok(with), Ok(without)) if with != without => {
                        surviving_path = Some(path);
                        break;
                    }
                    (Ok(_), Ok(_)) => {}
                    _ => break,
                }
            }
            if let Some(path) = surviving_path {
                evaluation.context_only.push(blocker.change);
                evaluation.surviving_paths.insert(blocker.change, path);
            } else {
                evaluation.confirmed_blockers.push(blocker.change);
            }
        }

        plan.evaluation = Some(evaluation);
        plan.phase = SelectiveRepairPhase::Evaluated;
        plan.generation += 1;
        plan.timestamps.updated_at = updated_at;
        self.save_selective_repair_plan(&plan)?;
        Ok(plan)
    }

    /// Atomically save the pending selective-repair plan after validating it.
    pub fn save_selective_repair_plan(
        &self,
        plan: &SelectiveRepairPlan,
    ) -> Result<(), RepositoryError> {
        plan.validate()?;
        let path = self.dot_dir.join(SELECTIVE_REPAIR_PLAN_FILE);
        let mut temp = tempfile::NamedTempFile::new_in(&self.dot_dir)?;
        serde_json::to_writer_pretty(temp.as_file_mut(), plan)?;
        temp.as_file_mut().write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| {
            RepositoryError::Io(std::io::Error::other(format!(
                "failed to persist selective repair plan: {error}"
            )))
        })?;
        self.sync_dot_dir()?;
        Ok(())
    }

    /// Load and validate the pending plan, returning `None` when none exists.
    pub fn load_selective_repair_plan(
        &self,
    ) -> Result<Option<SelectiveRepairPlan>, RepositoryError> {
        let path = self.dot_dir.join(SELECTIVE_REPAIR_PLAN_FILE);
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let plan: SelectiveRepairPlan = serde_json::from_slice(&bytes)?;
        plan.validate()?;
        Ok(Some(plan))
    }

    /// Remove the pending plan. This is idempotent and does not mutate graph state.
    pub fn remove_selective_repair_plan(&self) -> Result<(), RepositoryError> {
        let path = self.dot_dir.join(SELECTIVE_REPAIR_PLAN_FILE);
        match std::fs::remove_file(path) {
            Ok(()) => self.sync_dot_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> Hash {
        Merkle([byte; 32])
    }

    fn plan() -> SelectiveRepairPlan {
        let target = hash(1);
        let blocker = hash(2);
        let commuting = hash(3);
        let target_replacement = hash(11);
        let blocker_replacement = hash(12);
        SelectiveRepairPlan {
            version: SELECTIVE_REPAIR_PLAN_VERSION,
            plan_id: "repair-1".into(),
            generation: 0,
            phase: SelectiveRepairPhase::ReadyToPublish,
            view: "dev".into(),
            original_state: hash(9),
            original_order: vec![
                OrderedChange {
                    change: target,
                    order: 1,
                },
                OrderedChange {
                    change: blocker,
                    order: 2,
                },
                OrderedChange {
                    change: commuting,
                    order: 3,
                },
            ],
            target,
            blockers: vec![StructuralBlocker {
                change: blocker,
                references: vec![target],
            }],
            commuting: vec![commuting],
            replacements: BTreeMap::from([
                (
                    target,
                    Replacement {
                        original: target,
                        replacement: target_replacement,
                        patch_relink: Some(hash(20)),
                        provenance_relinks: vec![hash(21)],
                    },
                ),
                (
                    blocker,
                    Replacement {
                        original: blocker,
                        replacement: blocker_replacement,
                        patch_relink: Some(hash(23)),
                        provenance_relinks: vec![hash(22)],
                    },
                ),
            ]),
            replacement_order: vec![
                OrderedChange {
                    change: target_replacement,
                    order: 1,
                },
                OrderedChange {
                    change: blocker_replacement,
                    order: 2,
                },
            ],
            original_provenance: BTreeMap::from([
                (target, vec![hash(31)]),
                (blocker, vec![hash(32)]),
            ]),
            evaluation: None,
            timestamps: SelectiveRepairTimestamps {
                created_at: "2026-09-10T00:00:00Z".into(),
                updated_at: "2026-09-10T00:00:00Z".into(),
                completed_at: None,
            },
        }
    }

    #[test]
    fn valid_ready_plan_passes_validation() {
        assert!(plan().validate().is_ok());
    }

    #[test]
    fn rejects_unaccounted_later_change() {
        let mut plan = plan();
        plan.commuting.clear();
        assert!(plan.validate().is_err());
    }

    #[test]
    fn evaluated_plan_requires_complete_candidate_partition() {
        let mut plan = plan();
        plan.phase = SelectiveRepairPhase::Evaluated;
        plan.evaluation = Some(SelectiveRepairEvaluation {
            confirmed_blockers: vec![hash(2)],
            context_only: Vec::new(),
            surviving_paths: BTreeMap::new(),
        });
        assert!(plan.validate().is_ok());
        plan.evaluation.as_mut().unwrap().confirmed_blockers.clear();
        assert!(plan.validate().is_err());
    }

    #[test]
    fn rejects_incomplete_ready_plan() {
        let mut plan = plan();
        plan.replacements.remove(&hash(2));
        plan.replacement_order.pop();

        assert!(plan.validate().is_err());
    }

    #[test]
    fn json_is_versioned_and_newline_terminated_shape_is_round_trippable() {
        let plan = plan();
        let bytes = serde_json::to_vec(&plan).unwrap();
        let decoded: SelectiveRepairPlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, plan);
        assert_eq!(decoded.version, SELECTIVE_REPAIR_PLAN_VERSION);
    }

    #[test]
    fn pending_plan_persists_across_repository_reopen_and_removes_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let expected = plan();
        {
            let repo = Repository::init(temp.path()).unwrap();
            repo.save_selective_repair_plan(&expected).unwrap();
            assert_eq!(
                repo.load_selective_repair_plan().unwrap(),
                Some(expected.clone())
            );
        }
        {
            let repo = Repository::open(temp.path()).unwrap();
            assert_eq!(repo.load_selective_repair_plan().unwrap(), Some(expected));
            repo.remove_selective_repair_plan().unwrap();
            repo.remove_selective_repair_plan().unwrap();
            assert_eq!(repo.load_selective_repair_plan().unwrap(), None);
        }
    }
}

//! The `unrecord` command for removing changes from a view.
//!
//! This module implements the `atomic unrecord` command, which removes
//! the most recent change (or a specific change) from the current view's
//! change log. The change itself is NOT deleted from the change store —
//! it can be re-inserted later via `atomic insert`.
//!
//! # Usage
//!
//! ```text
//! atomic unrecord [OPTIONS] [CHANGE]
//!
//! Arguments:
//!   [CHANGE]  Hash or prefix of the change to unrecord (default: last change)
//!
//! Options:
//!   -n, --dry-run   Preview what would be unrecorded
//!   -h, --help      Print help information
//! ```
//!
//! # Examples
//!
//! Unrecord the most recent change:
//! ```text
//! $ atomic unrecord
//! Unrecorded: ABCDEF12 "Add feature file"
//! ```
//!
//! Unrecord a specific change by hash prefix:
//! ```text
//! $ atomic unrecord ABCDEF
//! Unrecorded: ABCDEF12 "Add feature file"
//! ```
//!
//! Preview without actually unrecording:
//! ```text
//! $ atomic unrecord --dry-run
//! Would unrecord: ABCDEF12 "Add feature file"
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use atomic_core::types::Base32;
use atomic_repository::history::HistoryOptions;
use atomic_repository::unrecord::UnrecordOptions;
use atomic_repository::{
    OrderedChange, Repository, SelectiveRepairPhase, SelectiveRepairPlan,
    SelectiveRepairTimestamps, StructuralBlocker, SELECTIVE_REPAIR_PLAN_VERSION,
};

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};
use crate::output::{print_success, print_warning};

/// Remove the last change from the current view.
///
/// The change is removed from the view's change log but NOT deleted
/// from the change store. It can be re-inserted later with `atomic insert`.
///
/// This is the inverse of `atomic record` — it "un-records" a change,
/// reverting the view to the state before that change was applied.
/// The working copy is NOT modified; files remain on disk as-is.
///
/// # Workflow
///
/// ```text
/// atomic record -m "oops"   # record a change
/// atomic unrecord            # remove it from the view
/// # fix the issue
/// atomic record -m "fixed"  # record the corrected version
/// ```
#[derive(Parser, Debug, Default)]
#[command(name = "unrecord")]
pub struct Unrecord {
    /// Hash or prefix of the change to unrecord.
    ///
    /// If not specified, the most recent change on the current view
    /// is unrecorded. Provide a hash prefix to unrecord a specific change.
    #[arg(value_name = "CHANGE")]
    pub change: Option<String>,

    /// Preview what would be unrecorded without doing it.
    #[arg(short = 'n', long = "dry-run")]
    pub dry_run: bool,

    /// Persist the read-only commutation analysis as a crash-safe repair plan.
    #[arg(long, requires = "change")]
    pub plan: bool,
}

impl Command for Unrecord {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(|e| match e {
            atomic_repository::RepositoryError::NotFound { path } => CliError::RepositoryNotFound {
                searched_path: path.into(),
            },
            other => CliError::Repository(other),
        })?;

        let options = if self.dry_run || self.plan {
            UnrecordOptions::dry_run()
        } else {
            UnrecordOptions::new()
        };

        let outcome = if let Some(ref prefix) = self.change {
            let hash = repo
                .find_change_by_prefix(prefix)
                .map_err(CliError::Repository)?
                .ok_or_else(|| CliError::InvalidArgument {
                    message: format!("Change not found: {prefix}"),
                })?;

            if self.dry_run || self.plan {
                let history = repo
                    .log(HistoryOptions::default())
                    .map_err(CliError::Repository)?;
                let target = history
                    .iter()
                    .find(|entry| entry.hash == hash)
                    .ok_or_else(|| CliError::InvalidArgument {
                        message: format!("Change {} is not in the current view", hash.to_base32()),
                    })?;

                let target_provenance_hashes = repo
                    .find_provenance_for_change(&hash)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(provenance_hash, _)| provenance_hash)
                    .collect::<Vec<_>>();
                let target_provenance = if target_provenance_hashes.is_empty() {
                    "none".to_string()
                } else {
                    target_provenance_hashes
                        .iter()
                        .map(|provenance_hash| provenance_hash.to_base32())
                        .collect::<Vec<_>>()
                        .join(",")
                };
                let mut repair_closure = BTreeSet::from([hash]);
                let mut blockers = Vec::new();
                let mut commuting_changes = Vec::new();
                let mut provenance_inventory = BTreeMap::from([(hash, target_provenance_hashes)]);
                println!(
                    "Commutation analysis for {} provenance={}:",
                    hash.to_base32(),
                    target_provenance
                );
                for entry in history
                    .iter()
                    .filter(|entry| entry.sequence > target.sequence)
                {
                    let change = repo.load_change(&entry.hash).map_err(|error| {
                        CliError::Internal(anyhow::anyhow!(
                            "Failed to load {}: {}",
                            entry.hash.to_base32(),
                            error
                        ))
                    })?;
                    let provenance = repo
                        .find_provenance_for_change(&entry.hash)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(provenance_hash, _)| provenance_hash)
                        .collect::<Vec<_>>();
                    let provenance_display = if provenance.is_empty() {
                        "none".to_string()
                    } else {
                        provenance
                            .iter()
                            .map(|provenance_hash| provenance_hash.to_base32())
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    let references = change
                        .structurally_referenced_changes()
                        .map_err(|error| {
                            CliError::Internal(anyhow::anyhow!(
                                "Failed to inspect {}: {}",
                                entry.hash.to_base32(),
                                error
                            ))
                        })?
                        .into_iter()
                        .filter(|referenced| repair_closure.contains(referenced))
                        .collect::<Vec<_>>();
                    if !references.is_empty() {
                        repair_closure.insert(entry.hash);
                        provenance_inventory.insert(entry.hash, provenance);
                        blockers.push(StructuralBlocker {
                            change: entry.hash,
                            references,
                        });
                        println!(
                            "  BLOCKS   {} provenance={}",
                            entry.hash.to_base32(),
                            provenance_display
                        );
                    } else {
                        commuting_changes.push(entry.hash);
                        println!(
                            "  COMMUTES {} provenance={}",
                            entry.hash.to_base32(),
                            provenance_display
                        );
                    }
                }
                println!(
                    "Summary: {} commuting, {} structurally dependent",
                    commuting_changes.len(),
                    blockers.len()
                );

                if self.plan {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_err(|error| CliError::Internal(anyhow::anyhow!(error)))?
                        .as_secs();
                    let view = repo.current_view().to_string();
                    let view_info = repo.get_view_info(&view).map_err(CliError::Repository)?;
                    let plan = SelectiveRepairPlan {
                        version: SELECTIVE_REPAIR_PLAN_VERSION,
                        plan_id: format!("repair-{}-{now}", &hash.to_base32()[..12]),
                        generation: 0,
                        phase: SelectiveRepairPhase::Planned,
                        view,
                        original_state: view_info.state,
                        original_order: history
                            .iter()
                            .map(|entry| OrderedChange {
                                change: entry.hash,
                                order: entry.sequence,
                            })
                            .collect(),
                        target: hash,
                        blockers,
                        commuting: commuting_changes,
                        replacements: BTreeMap::new(),
                        replacement_order: Vec::new(),
                        original_provenance: provenance_inventory,
                        evaluation: None,
                        timestamps: SelectiveRepairTimestamps {
                            created_at: now.to_string(),
                            updated_at: now.to_string(),
                            completed_at: None,
                        },
                    };
                    repo.save_selective_repair_plan(&plan)
                        .map_err(CliError::Repository)?;
                    let persisted = repo
                        .load_selective_repair_plan()
                        .map_err(CliError::Repository)?
                        .ok_or_else(|| {
                            CliError::Internal(anyhow::anyhow!(
                                "selective repair plan disappeared after persistence"
                            ))
                        })?;
                    println!(
                        "Saved repair plan {} generation={} phase={:?}",
                        persisted.plan_id, persisted.generation, persisted.phase
                    );
                }
                return Ok(());
            }

            return Err(CliError::InvalidArgument {
                message: "Selective unrecord is experimental. Run with --dry-run to inspect commutation first."
                    .to_string(),
            });
        } else {
            // Unrecord the most recent change
            repo.unrecord_last(options).map_err(|e| match e {
                atomic_repository::RepositoryError::Unrecord(msg)
                    if msg.contains("empty") || msg.contains("Empty") =>
                {
                    CliError::InvalidArgument {
                        message: "View is empty — nothing to unrecord".to_string(),
                    }
                }
                other => CliError::Repository(other),
            })?
        };

        if outcome.was_dry_run {
            for hash in &outcome.unrecorded {
                print_warning(&format!("Would unrecord: {}", hash.to_base32()));
            }
        } else {
            for hash in &outcome.unrecorded {
                print_success(&format!("Unrecorded: {}", hash.to_base32()));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default() {
        let cmd = Unrecord::default();
        assert!(cmd.change.is_none());
        assert!(!cmd.dry_run);
    }
}

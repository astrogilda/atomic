//! Isolate a structurally connected patch subgraph for selective repair.

use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use atomic_repository::Repository;

use crate::commands::{find_repository_root, Command, Unrecord};
use crate::error::{CliError, CliResult};

#[derive(Parser, Debug, Default)]
#[command(name = "isolate")]
pub struct Isolate {
    /// Change hash or unambiguous prefix to isolate.
    #[arg(value_name = "CHANGE")]
    pub change: Option<String>,

    /// View whose closure should be analyzed (defaults to the current view).
    #[arg(long, value_name = "VIEW")]
    pub from: Option<String>,

    /// Analyze without persisting a repair plan.
    #[arg(long)]
    pub dry_run: bool,

    /// Evaluate candidate blockers under a target-excluded graph filter.
    #[arg(long)]
    pub evaluate: bool,

    /// Show the active isolation plan.
    #[arg(long)]
    pub status: bool,

    /// Abort and remove the active plan before publication.
    #[arg(long)]
    pub abort: bool,

    /// Continue the active plan after filesystem cleanup.
    #[arg(long)]
    pub finish: bool,
}

impl Command for Isolate {
    fn run(&self) -> CliResult<()> {
        let selected = usize::from(self.status)
            + usize::from(self.evaluate)
            + usize::from(self.abort)
            + usize::from(self.finish)
            + usize::from(self.change.is_some());
        if selected != 1 {
            return Err(CliError::InvalidArgument {
                message: "Use exactly one of CHANGE, --evaluate, --status, --abort, or --finish"
                    .to_string(),
            });
        }

        let repo_root = find_repository_root()?;
        let repo = Repository::open(&repo_root).map_err(CliError::Repository)?;

        if let Some(from) = self.from.as_deref() {
            if from != repo.current_view() {
                return Err(CliError::InvalidArgument {
                    message: format!(
                        "isolation currently requires '{}' to be checked out; current view is '{}'",
                        from,
                        repo.current_view()
                    ),
                });
            }
        }

        if self.status {
            return match repo
                .load_selective_repair_plan()
                .map_err(CliError::Repository)?
            {
                Some(plan) => {
                    println!("Isolation plan: {}", plan.plan_id);
                    println!("  phase: {:?}", plan.phase);
                    println!("  generation: {}", plan.generation);
                    println!("  view: {}", plan.view);
                    println!("  target: {}", plan.target.to_base32());
                    println!("  blockers: {}", plan.blockers.len());
                    println!("  commuting unchanged: {}", plan.commuting.len());
                    println!("  replacements: {}", plan.replacements.len());
                    if let Some(evaluation) = &plan.evaluation {
                        println!(
                            "  evaluated blockers: {}",
                            evaluation.confirmed_blockers.len()
                        );
                        println!(
                            "  context-only candidates: {}",
                            evaluation.context_only.len()
                        );
                    }
                    Ok(())
                }
                None => Err(CliError::InvalidArgument {
                    message: "No active isolation plan".to_string(),
                }),
            };
        }

        if self.evaluate {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| CliError::Internal(anyhow::anyhow!(error)))?
                .as_secs()
                .to_string();
            let plan = repo
                .evaluate_selective_repair_plan(now)
                .map_err(CliError::Repository)?;
            let evaluation = plan.evaluation.expect("evaluated phase carries evidence");
            println!("Evaluated isolation plan {}", plan.plan_id);
            println!(
                "  confirmed blockers: {}",
                evaluation.confirmed_blockers.len()
            );
            println!(
                "  context-only candidates: {}",
                evaluation.context_only.len()
            );
            for (change, path) in evaluation.surviving_paths {
                println!("  survives {} via {}", change.to_base32(), path);
            }
            return Ok(());
        }

        if self.abort {
            let plan = repo
                .load_selective_repair_plan()
                .map_err(CliError::Repository)?
                .ok_or_else(|| CliError::InvalidArgument {
                    message: "No active isolation plan".to_string(),
                })?;
            if matches!(
                plan.phase,
                atomic_repository::SelectiveRepairPhase::ViewPublished
                    | atomic_repository::SelectiveRepairPhase::Completed
            ) {
                return Err(CliError::InvalidArgument {
                    message: "Cannot abort an isolation plan after publication".to_string(),
                });
            }
            repo.remove_selective_repair_plan()
                .map_err(CliError::Repository)?;
            println!("Aborted isolation plan {}", plan.plan_id);
            return Ok(());
        }

        if self.finish {
            let plan = repo
                .load_selective_repair_plan()
                .map_err(CliError::Repository)?
                .ok_or_else(|| CliError::InvalidArgument {
                    message: "No active isolation plan".to_string(),
                })?;
            return Err(CliError::InvalidArgument {
                message: format!(
                    "Isolation plan {} is {:?}; replacement capture is not enabled yet, so no view was mutated",
                    plan.plan_id, plan.phase
                ),
            });
        }

        drop(repo);
        Unrecord {
            change: self.change.clone(),
            dry_run: self.dry_run,
            plan: !self.dry_run,
        }
        .run()
    }
}

use atomic_core::types::Base32;

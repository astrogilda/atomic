//! Shared CB-5B entry adapter for local working-copy commands.

use atomic_repository::{
    Repository, WorkspaceRemediation, WorkspaceTxn, WorkspaceTxnMode, WorkspaceTxnStart,
};

use crate::error::{CliError, CliResult};

/// Enter a stable workspace transaction or convert remediation into one CLI refusal.
pub(crate) fn enter_workspace(
    repository: &mut Repository,
    mode: WorkspaceTxnMode,
) -> CliResult<WorkspaceTxn> {
    match repository
        .begin_workspace_txn(mode)
        .map_err(CliError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(transaction),
        WorkspaceTxnStart::Remediation(remediation) => Err(remediation_error(remediation)),
    }
}

/// Observe a workspace boundary while allowing forensic commands to report unsafe state.
pub(crate) fn observe_workspace(
    repository: &mut Repository,
) -> CliResult<Result<WorkspaceTxn, WorkspaceRemediation>> {
    match repository
        .begin_workspace_txn(WorkspaceTxnMode::Observe)
        .map_err(CliError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(Ok(transaction)),
        WorkspaceTxnStart::Remediation(remediation) => Ok(Err(remediation)),
    }
}

pub(crate) fn remediation_error(remediation: WorkspaceRemediation) -> CliError {
    CliError::StaleBaseline {
        report: format!("workspace reconciliation required: {remediation:#?}"),
    }
}

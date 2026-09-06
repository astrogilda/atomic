use super::*;

use atomic_core::pristine::{GraphVisibilityClosure, ViewMembershipSet, ViewState};

fn map_pristine_error(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

/// Collect the ordered direct membership of `view` and its full parent chain.
///
/// This is the canonical repository boundary for membership-only operations.
/// It deliberately does not expand dependencies and therefore must be
/// validated with [`graph_visibility_from_membership`] before graph traversal.
pub fn view_membership<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
) -> Result<ViewMembershipSet, RepositoryError> {
    txn.view_membership_set(view).map_err(map_pristine_error)
}

/// Collect full ancestor membership plus a prefix of the leaf view's own log.
///
/// `max_sequence` is an exclusive bound on `view` only. Every ancestor is
/// included completely, preserving root-to-leaf view-log order and first
/// occurrence semantics.
pub fn view_membership_at_sequence<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    max_sequence: u64,
) -> Result<ViewMembershipSet, RepositoryError> {
    let chain = txn
        .resolve_full_view_chain(view)
        .map_err(map_pristine_error)?;
    let mut ordered = Vec::new();
    for member_view in chain {
        let iter = txn
            .iter_changes(&member_view, 0)
            .map_err(map_pristine_error)?;
        for result in iter {
            let (sequence, change_id, _merkle) = result.map_err(map_pristine_error)?;
            if member_view.id == view.id && sequence >= max_sequence {
                break;
            }
            ordered.push(change_id);
        }
    }
    Ok(ViewMembershipSet::from_ordered(ordered))
}

/// Validate direct membership into the dependency closure required by graph
/// traversal and path-lifecycle projection.
///
/// Validation fails closed when dependency metadata is missing, incomplete, or
/// cyclic, or when a dependency is not registered locally. The pristine error
/// text is preserved so callers receive the repair-relevant details.
pub fn graph_visibility_from_membership<T: GraphTxnT>(
    txn: &T,
    membership: &ViewMembershipSet,
) -> Result<GraphVisibilityClosure, RepositoryError> {
    GraphVisibilityClosure::try_from_membership(txn, membership).map_err(map_pristine_error)
}

/// Build validated graph visibility for `view` and its full parent chain.
pub fn graph_visibility_closure<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
) -> Result<GraphVisibilityClosure, RepositoryError> {
    super::effective_projection_closure(txn, view)
}

/// Typed compatibility alias for callers that still use the former helper name.
pub fn collect_view_change_ids<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
) -> Result<ViewMembershipSet, RepositoryError> {
    view_membership(txn, view)
}

/// Typed compatibility alias for direct visible membership.
pub fn collect_visible_change_ids<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
) -> Result<ViewMembershipSet, RepositoryError> {
    view_membership(txn, view)
}

/// Typed compatibility alias for validated graph visibility.
pub fn collect_visible_change_ids_with_deps<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
) -> Result<GraphVisibilityClosure, RepositoryError> {
    graph_visibility_closure(txn, view)
}

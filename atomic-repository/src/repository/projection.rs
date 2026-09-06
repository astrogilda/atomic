use super::*;

use atomic_core::pristine::EffectiveProjectionClosure;

fn map_pristine_error(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

/// Build the one canonical visibility closure for a view projection.
///
/// The closure includes the full parent-chain membership and every transitive
/// dependency, is deterministic and dependency-first, and fails closed on
/// missing, incomplete, or cyclic dependency metadata. Graph traversal,
/// attributes, semantic state, SetId, export, and bindings must all derive
/// visibility from this value.
pub fn effective_projection_closure<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<EffectiveProjectionClosure, RepositoryError> {
    let membership = view_membership(txn, view)?;
    EffectiveProjectionClosure::try_from_membership(txn, &membership).map_err(map_pristine_error)
}

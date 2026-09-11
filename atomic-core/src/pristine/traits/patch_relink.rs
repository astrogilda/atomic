//! Persistent patch-alias and graph-node relink lookups.

use crate::pristine::error::PristineError;
use crate::types::{GraphNode, NodeId};

/// The recorded outcome for a graph node belonging to a replaced patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchRelinkTarget {
    /// The old node has a corresponding node in the replacement patch.
    Mapped(GraphNode<NodeId>),
    /// The old node was deliberately removed by the replacement patch.
    Removed,
}

/// Read access to persisted patch replacement metadata.
pub trait PatchRelinkTxnT {
    /// Return the replacement patch for `old_change`, when one is recorded.
    fn get_patch_alias(&self, old_change: NodeId) -> Result<Option<NodeId>, PristineError>;

    /// Return patches directly replaced by `replacement`.
    fn get_patch_alias_sources(&self, replacement: NodeId) -> Result<Vec<NodeId>, PristineError>;

    /// Return the replacement outcome for an exact old graph node.
    fn get_patch_relink(
        &self,
        old_node: GraphNode<NodeId>,
    ) -> Result<Option<PatchRelinkTarget>, PristineError>;
}

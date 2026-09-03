//! View-scoped graph wrapper that filters edge traversal by visibility.
//!
//! `ViewGraph` wraps any `GraphTxnT` implementor and a validated
//! [`GraphVisibilityClosure`]. When iterating adjacent edges, only edges whose
//! `introduced_by` is in the closure (or is ROOT) are returned.
//!
//! Position lookups (`find_block`, `find_block_end`) are NOT filtered
//! because they are structural — a vertex exists at a position regardless
//! of which view introduced edges to it.
//!
//! This replaces the old `OverlayTxn` which unioned `STACK_GRAPH` with
//! `GRAPH`. In the ambient graph model, there is only `GRAPH`, and
//! `ViewGraph` controls which edges are visible per-view.

use crate::pristine::{
    FileIndexEntry, FileIndexMetadata, GraphTxnT, GraphVisibilityClosure, InodeAdjState,
    InodeGraphOps, PristineError, TreeTxnT,
};
use crate::types::{EdgeFlags, GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge};

/// A view-scoped graph wrapper that filters edge traversal by visibility.
///
/// `ViewGraph` wraps any `GraphTxnT` implementor and a validated visibility
/// closure. When iterating adjacent edges, only edges whose `introduced_by` is
/// in the closure (or is ROOT) are returned.
///
/// Position lookups (`find_block`, `find_block_end`) are NOT filtered
/// because they are structural — a vertex exists at a position regardless
/// of which view introduced edges to it.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::pristine::{GraphVisibilityClosure, ViewGraph};
///
/// let membership = txn.view_membership_set(&view)?;
/// let visibility = GraphVisibilityClosure::try_from_membership(&txn, &membership)?;
/// let vg = ViewGraph::new(&txn, visibility);
///
/// // iter_adjacent now only returns edges from the view's changes
/// let edges = vg.iter_adjacent(node, min_flag, max_flag)?;
/// ```
pub struct ViewGraph<'a, T> {
    inner: &'a T,
    visibility: GraphVisibilityClosure,
}

impl<'a, T> ViewGraph<'a, T> {
    /// Create a new view-scoped graph wrapper.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying transaction implementing `GraphTxnT`
    /// * `visibility` - Validated dependency closure whose edges are visible
    pub fn new(inner: &'a T, visibility: GraphVisibilityClosure) -> Self {
        Self { inner, visibility }
    }

    /// Get a reference to the inner transaction.
    pub fn inner(&self) -> &T {
        self.inner
    }

    /// Check whether a change is visible in this view.
    ///
    /// ROOT is always visible regardless of the filter set.
    #[cfg(test)]
    fn is_visible(&self, change_id: NodeId) -> bool {
        change_id == NodeId::ROOT || self.visibility.contains(change_id)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// InodeGraphOps — delegate to inner txn (INODE_GRAPH is unfiltered)
// ─────────────────────────────────────────────────────────────────────────

impl<'a, T: InodeGraphOps> InodeGraphOps for ViewGraph<'a, T> {
    type InodeError = T::InodeError;

    fn init_inode_adj(
        &self,
        inode: Inode,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<InodeAdjState, Self::InodeError> {
        self.inner.init_inode_adj(inode, node, min_flag, max_flag)
    }

    fn next_inode_adj(
        &self,
        adj: &mut InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
        if adj.is_exhausted() {
            return None;
        }

        // Filter inode edges by the view's visible change set,
        // same as iter_adjacent does for the global GRAPH.
        loop {
            match self.inner.next_inode_adj(adj) {
                Some(Ok(edge)) => {
                    let introduced = edge.introduced_by();
                    if introduced.is_root() || self.visibility.contains(introduced) {
                        return Some(Ok(edge));
                    }
                    // Edge from a non-visible change — skip it
                    continue;
                }
                Some(Err(error)) => {
                    adj.mark_exhausted();
                    return Some(Err(error));
                }
                None => return None,
            }
        }
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        self.inner.find_block_in_inode(inode, pos)
    }

    fn find_block_end_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        self.inner.find_block_end_in_inode(inode, pos)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        self.inner.count_inode_vertices(inode)
    }

    fn inode_graph_is_populated(&self, inode: Inode) -> Result<bool, Self::InodeError> {
        self.inner.inode_graph_is_populated(inode)
    }

    fn inode_graph_needs_view_filter(&self) -> bool {
        // ViewGraph now filters inode edges in next_inode_adj,
        // so callers can safely use the INODE_GRAPH fast path.
        false
    }
}

/// Filtered adjacency iterator that only yields edges from visible changes.
pub struct FilteredAdj<I> {
    inner: I,
    visibility: GraphVisibilityClosure,
    exhausted: bool,
}

impl<I> Iterator for FilteredAdj<I>
where
    I: Iterator<Item = Result<SerializedGraphEdge, PristineError>>,
{
    type Item = Result<SerializedGraphEdge, PristineError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }

        loop {
            match self.inner.next() {
                Some(Ok(edge)) => {
                    let introduced_by = edge.introduced_by();
                    if introduced_by == NodeId::ROOT || self.visibility.contains(introduced_by) {
                        return Some(Ok(edge));
                    }
                    // Skip edges not visible in this view
                    continue;
                }
                Some(Err(error)) => {
                    self.exhausted = true;
                    return Some(Err(error));
                }
                None => {
                    self.exhausted = true;
                    return None;
                }
            }
        }
    }
}

impl<'a, T: GraphTxnT> GraphTxnT for ViewGraph<'a, T> {
    type Adj = FilteredAdj<T::Adj>;

    /// Iterate adjacent edges, filtering by visibility.
    ///
    /// Only edges whose `introduced_by` is ROOT or is in the visible set
    /// are yielded. All other edges are silently skipped.
    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, PristineError> {
        let inner_iter = self.inner.iter_adjacent(node, min_flag, max_flag)?;
        Ok(FilteredAdj {
            inner: inner_iter,
            visibility: self.visibility.clone(),
            exhausted: false,
        })
    }

    /// Structural lookup — no filtering. Delegates to inner.
    fn find_block(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        self.inner.find_block(pos)
    }

    /// Structural lookup — no filtering. Delegates to inner.
    fn find_block_end(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        self.inner.find_block_end(pos)
    }

    /// Structural check — no filtering. Delegates to inner.
    fn has_vertex(&self, node: GraphNode<NodeId>) -> Result<bool, PristineError> {
        self.inner.has_vertex(node)
    }

    /// ID mapping — no filtering. Delegates to inner.
    fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError> {
        self.inner.get_external(id)
    }

    /// ID mapping — no filtering. Delegates to inner.
    fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, PristineError> {
        self.inner.get_internal(hash)
    }

    /// Registered change listing — no filtering. Delegates to inner.
    fn list_registered_changes(&self) -> Result<Vec<(NodeId, Hash)>, PristineError> {
        self.inner.list_registered_changes()
    }

    /// Node type lookup — no filtering. Delegates to inner.
    fn get_node_type(&self, node_id: NodeId) -> Result<Option<u8>, PristineError> {
        self.inner.get_node_type(node_id)
    }

    /// Reverse dependency lookup — no filtering. Delegates to inner.
    fn get_rev_deps(&self, dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_deps(dep_id)
    }

    /// Indexed normal change dependency lookup — no filtering. Delegates to inner.
    fn get_change_deps(&self, change_id: NodeId) -> Result<Vec<Hash>, PristineError> {
        self.inner.get_change_deps(change_id)
    }

    /// Indexed dependency count lookup — no filtering. Delegates to inner.
    fn change_deps_indexed_count(&self, change_id: NodeId) -> Result<Option<u64>, PristineError> {
        self.inner.change_deps_indexed_count(change_id)
    }

    /// Reverse indexed normal change dependency lookup — no filtering. Delegates to inner.
    fn get_rev_change_deps(&self, dep_hash: &Hash) -> Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_change_deps(dep_hash)
    }

    /// Graph presence check — no filtering. Delegates to inner.
    fn has_change_in_graph(&self, change_id: NodeId) -> Result<bool, PristineError> {
        self.inner.has_change_in_graph(change_id)
    }
}

impl<'a, T: TreeTxnT> TreeTxnT for ViewGraph<'a, T> {
    fn get_inode(&self, path: &str) -> Result<Option<Inode>, PristineError> {
        self.inner.get_inode(path)
    }

    fn get_directory_flags(&self, inode: Inode) -> Result<Option<u8>, PristineError> {
        self.inner.get_directory_flags(inode)
    }

    fn get_path(&self, inode: Inode) -> Result<Option<String>, PristineError> {
        self.inner.get_path(inode)
    }

    fn inode_position(&self, inode: Inode) -> Result<Option<Position<NodeId>>, PristineError> {
        self.inner.inode_position(inode)
    }

    fn position_inode(&self, pos: Position<NodeId>) -> Result<Option<Inode>, PristineError> {
        self.inner.position_inode(pos)
    }

    fn iter_tree(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>, PristineError>
    {
        self.inner.iter_tree()
    }

    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> Result<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
        PristineError,
    > {
        self.inner.iter_inode_vertices(inode)
    }

    fn get_file_index(&self, path: &str) -> Result<Option<FileIndexMetadata>, PristineError> {
        self.inner.get_file_index(path)
    }

    fn iter_file_index(&self) -> Result<Vec<FileIndexEntry>, PristineError> {
        self.inner.iter_file_index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;
    use std::cell::Cell;

    fn visible_edge() -> SerializedGraphEdge {
        SerializedGraphEdge::new(
            EdgeFlags::BLOCK,
            Position::new(NodeId::ROOT, ChangePosition::new(0)),
            NodeId::ROOT,
        )
    }

    #[test]
    fn filtered_adj_is_terminal_after_underlying_error() {
        let error = PristineError::BlockNotFound { change: 1, pos: 0 };
        let mut adj = FilteredAdj {
            inner: vec![Err(error), Ok(visible_edge())].into_iter(),
            visibility: GraphVisibilityClosure::empty(),
            exhausted: false,
        };

        assert!(matches!(adj.next(), Some(Err(_))));
        assert!(adj.next().is_none());
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ScriptedInodeError;

    impl std::fmt::Display for ScriptedInodeError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("scripted inode error")
        }
    }

    impl std::error::Error for ScriptedInodeError {}

    struct ScriptedInodeTxn {
        calls: Cell<usize>,
    }

    impl InodeGraphOps for ScriptedInodeTxn {
        type InodeError = ScriptedInodeError;

        fn init_inode_adj(
            &self,
            inode: Inode,
            node: GraphNode<NodeId>,
            min_flag: EdgeFlags,
            max_flag: EdgeFlags,
        ) -> Result<InodeAdjState, Self::InodeError> {
            Ok(InodeAdjState::new(inode, node, min_flag, max_flag))
        }

        fn next_inode_adj(
            &self,
            _adj: &mut InodeAdjState,
        ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            if call == 0 {
                Some(Err(ScriptedInodeError))
            } else {
                Some(Ok(visible_edge()))
            }
        }

        fn find_block_in_inode(
            &self,
            _inode: Inode,
            _pos: Position<NodeId>,
        ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
            Ok(None)
        }

        fn count_inode_vertices(&self, _inode: Inode) -> Result<usize, Self::InodeError> {
            Ok(0)
        }
    }

    #[test]
    fn filtered_inode_adj_marks_state_terminal_after_underlying_error() {
        let inner = ScriptedInodeTxn {
            calls: Cell::new(0),
        };
        let graph = ViewGraph::new(&inner, GraphVisibilityClosure::empty());
        let mut adj = graph
            .init_inode_adj(
                Inode::new(1),
                GraphNode::ROOT,
                EdgeFlags::empty(),
                EdgeFlags::all(),
            )
            .unwrap();

        assert!(matches!(graph.next_inode_adj(&mut adj), Some(Err(_))));
        assert!(adj.is_exhausted());
        assert!(graph.next_inode_adj(&mut adj).is_none());
        assert_eq!(inner.calls.get(), 1);
    }

    #[test]
    fn test_is_visible_root_always_visible() {
        // We can't easily construct a full GraphTxnT mock here, but we can
        // test the is_visible logic directly.
        struct DummyTxn;

        let vg: ViewGraph<'_, DummyTxn> = ViewGraph {
            inner: &DummyTxn,
            visibility: GraphVisibilityClosure::empty(),
        };

        // ROOT is always visible even with an empty filter
        assert!(vg.is_visible(NodeId::ROOT));
    }

    #[test]
    fn test_is_visible_checks_set() {
        struct DummyTxn;

        let visibility =
            GraphVisibilityClosure::from_ordered_unchecked([NodeId::new(42), NodeId::new(99)]);

        let vg: ViewGraph<'_, DummyTxn> = ViewGraph {
            inner: &DummyTxn,
            visibility,
        };

        assert!(vg.is_visible(NodeId::new(42)));
        assert!(vg.is_visible(NodeId::new(99)));
        assert!(!vg.is_visible(NodeId::new(1)));
        assert!(!vg.is_visible(NodeId::new(100)));
        // ROOT is always visible
        assert!(vg.is_visible(NodeId::ROOT));
    }

    #[test]
    fn test_inner_returns_reference() {
        struct DummyTxn(u32);

        let txn = DummyTxn(123);
        let vg = ViewGraph {
            inner: &txn,
            visibility: GraphVisibilityClosure::empty(),
        };

        assert_eq!(vg.inner().0, 123);
    }
}

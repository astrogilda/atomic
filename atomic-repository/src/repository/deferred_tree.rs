use super::*;

use crate::tracking::{
    TreeProjectionError, TreeProjectionKind, TreeProjectionOperation, TreeProjectionPlan,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;

const DEFERRED_TREE_JOURNAL: &str = "deferred-tree-ops.json";
const DEFERRED_TREE_JOURNAL_VERSION: u32 = 1;
const DEFERRED_TREE_ALIGNMENT_PENDING: &str = "deferred-tree-alignment.pending";
const DEFERRED_TREE_ALIGNMENT_LOCK: &str = "deferred-tree-alignment.lock";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum DeferredTreeAction {
    Set { path: String },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct DeferredTreeOp {
    /// Change whose visibility activates this TREE operation.
    change: Hash,
    /// Stable inode position, stored with an external change hash so the
    /// journal never depends on process-local lookup state.
    inode: Position<Hash>,
    /// Path visible before this inode's first journaled operation. Only the
    /// first event for an inode uses this baseline; later events are selected
    /// solely by change visibility.
    baseline_path: Option<String>,
    /// Stable node kind carried by operation metadata. Legacy journals may not
    /// contain it and must prove the kind through DIRECTORIES instead.
    #[serde(default)]
    directory: Option<bool>,
    action: DeferredTreeAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeferredTreeJournal {
    version: u32,
    #[serde(default)]
    ops: Vec<DeferredTreeOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingTreeAlignment {
    version: u32,
    source_view: String,
    target_view: String,
}

impl Default for DeferredTreeJournal {
    fn default() -> Self {
        Self {
            version: DEFERRED_TREE_JOURNAL_VERSION,
            ops: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct DesiredTreePath {
    desired_path: Option<String>,
    last_present_path: Option<String>,
    known_paths: HashSet<String>,
    deleted: bool,
    /// Concurrent, causally maximal actions disagree. Until CB-N6 can retain
    /// multiple path claims, projection preserves the current cache path.
    ambiguous: bool,
    directory: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TreeProjection {
    pub(super) present: HashMap<String, OutputItem>,
    pub(super) absent: Vec<MaterializedEntry>,
}

fn projected_parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn projection_error(error: TreeProjectionError) -> RepositoryError {
    match error {
        TreeProjectionError::Database(message) => RepositoryError::Database(message),
        TreeProjectionError::IncompleteMetadata(message)
        | TreeProjectionError::Conflict(message) => RepositoryError::InvalidOperation { message },
    }
}

fn desired_tree_paths<F>(
    ops: &[DeferredTreeOp],
    visible_changes: &HashSet<Hash>,
    mut depends_on: F,
) -> Result<HashMap<Position<Hash>, DesiredTreePath>, RepositoryError>
where
    F: FnMut(Hash, Hash) -> Result<bool, RepositoryError>,
{
    let mut by_inode: HashMap<Position<Hash>, Vec<(usize, &DeferredTreeOp)>> = HashMap::new();
    for (order, op) in ops.iter().enumerate() {
        by_inode.entry(op.inode).or_default().push((order, op));
    }

    let mut desired = HashMap::new();
    for (inode, inode_ops) in by_inode {
        let baseline = inode_ops
            .iter()
            .find_map(|(_, op)| op.baseline_path.clone());
        let mut known_paths: HashSet<String> = inode_ops
            .iter()
            .filter_map(|(_, op)| op.baseline_path.clone())
            .collect();
        let mut directory = None;
        for (_, op) in &inode_ops {
            if let DeferredTreeAction::Set { path } = &op.action {
                known_paths.insert(path.clone());
            }
            if let Some(candidate) = op.directory {
                if directory.is_some_and(|existing| existing != candidate) {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection metadata changes inode {} from file to directory",
                            inode.pos.get()
                        ),
                    });
                }
                directory = Some(candidate);
            }
        }

        let visible: Vec<(usize, &DeferredTreeOp)> = inode_ops
            .into_iter()
            .filter(|(_, op)| visible_changes.contains(&op.change))
            .collect();
        let mut maximal = Vec::new();
        for (index, candidate) in &visible {
            let mut superseded = false;
            for (other_index, other) in &visible {
                if index == other_index {
                    continue;
                }
                if (candidate.change == other.change && other_index > index)
                    || (candidate.change != other.change
                        && depends_on(other.change, candidate.change)?)
                {
                    superseded = true;
                    break;
                }
            }
            if !superseded {
                maximal.push((*index, *candidate));
            }
        }

        let (desired_path, last_present_path, deleted, ambiguous) = if maximal.is_empty() {
            (baseline.clone(), baseline, false, false)
        } else {
            let first = &maximal[0].1.action;
            if maximal.iter().any(|(_, op)| &op.action != first) {
                // CB-N6 will retain every path claim explicitly. Until then,
                // do not let journal order pick a winner and do not turn a
                // previously insertable conflict into a hard failure. Callers
                // preserve the current projected cache path for this inode.
                (None, baseline.clone(), false, true)
            } else {
                match first {
                    DeferredTreeAction::Set { path } => {
                        (Some(path.clone()), Some(path.clone()), false, false)
                    }
                    DeferredTreeAction::Delete => {
                        let mut predecessors = Vec::new();
                        for (set_index, set_op) in &visible {
                            if !matches!(set_op.action, DeferredTreeAction::Set { .. }) {
                                continue;
                            }
                            let mut precedes_delete = false;
                            for (delete_index, delete_op) in &maximal {
                                if (set_op.change == delete_op.change && set_index < delete_index)
                                    || (set_op.change != delete_op.change
                                        && depends_on(delete_op.change, set_op.change)?)
                                {
                                    precedes_delete = true;
                                    break;
                                }
                            }
                            if precedes_delete {
                                predecessors.push((*set_index, *set_op));
                            }
                        }
                        let mut maximal_predecessors = Vec::new();
                        for (index, candidate) in &predecessors {
                            let mut superseded = false;
                            for (other_index, other) in &predecessors {
                                if index == other_index {
                                    continue;
                                }
                                if (candidate.change == other.change && other_index > index)
                                    || (candidate.change != other.change
                                        && depends_on(other.change, candidate.change)?)
                                {
                                    superseded = true;
                                    break;
                                }
                            }
                            if !superseded {
                                maximal_predecessors.push(*candidate);
                            }
                        }
                        let prior_paths: HashSet<String> = maximal_predecessors
                            .iter()
                            .filter_map(|op| match &op.action {
                                DeferredTreeAction::Set { path } => Some(path.clone()),
                                DeferredTreeAction::Delete => None,
                            })
                            .collect();
                        if prior_paths.len() > 1 {
                            return Err(RepositoryError::InvalidOperation {
                                message: format!(
                                    "delete for inode {} has causally ambiguous prior paths",
                                    inode.pos.get()
                                ),
                            });
                        }
                        (
                            None,
                            prior_paths.into_iter().next().or(baseline),
                            true,
                            false,
                        )
                    }
                }
            }
        };

        desired.insert(
            inode,
            DesiredTreePath {
                desired_path,
                last_present_path,
                known_paths,
                deleted,
                ambiguous,
                directory,
            },
        );
    }

    // Multiple visible inodes may currently claim one path. Until CB-N6 adds
    // PATH_CLAIMS, REV_TREE is the compatibility source for surfacing those
    // name conflicts. Keep every desired inode here; rejecting the projection
    // would turn an existing honest conflict into an insert failure.
    Ok(desired)
}

fn change_depends_on<T: GraphTxnT>(
    txn: &T,
    descendant: Hash,
    ancestor: Hash,
    memo: &mut HashMap<(Hash, Hash), bool>,
) -> Result<bool, RepositoryError> {
    if descendant == ancestor {
        return Ok(true);
    }
    if let Some(result) = memo.get(&(descendant, ancestor)) {
        return Ok(*result);
    }
    let descendant_id = txn
        .get_internal(&descendant)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "tree projection references unknown change {}",
                descendant.to_base32()
            ),
        })?;
    let dependencies = txn
        .get_indexed_change_deps(descendant_id)
        .map_err(|error| RepositoryError::InvalidOperation {
            message: error.to_string(),
        })?;
    for dependency in dependencies {
        if dependency == ancestor || change_depends_on(txn, dependency, ancestor, memo)? {
            memo.insert((descendant, ancestor), true);
            return Ok(true);
        }
    }
    memo.insert((descendant, ancestor), false);
    Ok(false)
}

fn desired_tree_paths_for_txn<T: GraphTxnT>(
    txn: &T,
    ops: &[DeferredTreeOp],
    visible_changes: &HashSet<Hash>,
) -> Result<HashMap<Position<Hash>, DesiredTreePath>, RepositoryError> {
    let mut memo = HashMap::new();
    desired_tree_paths(ops, visible_changes, |descendant, ancestor| {
        change_depends_on(txn, descendant, ancestor, &mut memo)
    })
}

fn causally_order_tree_ops<T: GraphTxnT>(
    txn: &T,
    ops: &[DeferredTreeOp],
) -> Result<Vec<DeferredTreeOp>, RepositoryError> {
    fn visit<T: GraphTxnT>(
        txn: &T,
        change: Hash,
        groups: &HashMap<Hash, Vec<DeferredTreeOp>>,
        visiting: &mut HashSet<Hash>,
        visited: &mut HashSet<Hash>,
        ordered: &mut Vec<DeferredTreeOp>,
    ) -> Result<(), RepositoryError> {
        if visited.contains(&change) {
            return Ok(());
        }
        if !visiting.insert(change) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "deferred TREE lifecycle contains a dependency cycle at {}",
                    change.to_base32()
                ),
            });
        }

        let change_id = txn
            .get_internal(&change)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "deferred TREE lifecycle references unknown change {}",
                    change.to_base32()
                ),
            })?;
        for dependency in txn.get_indexed_change_deps(change_id).map_err(|e| {
            RepositoryError::InvalidOperation {
                message: e.to_string(),
            }
        })? {
            if groups.contains_key(&dependency) {
                visit(txn, dependency, groups, visiting, visited, ordered)?;
            }
        }

        visiting.remove(&change);
        visited.insert(change);
        ordered.extend(
            groups
                .get(&change)
                .expect("visited deferred TREE change has an operation group")
                .iter()
                .cloned(),
        );
        Ok(())
    }

    let mut group_order = Vec::new();
    let mut groups: HashMap<Hash, Vec<DeferredTreeOp>> = HashMap::new();
    for op in ops {
        if !groups.contains_key(&op.change) {
            group_order.push(op.change);
        }
        groups.entry(op.change).or_default().push(op.clone());
    }

    let mut ordered = Vec::with_capacity(ops.len());
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for change in group_order {
        visit(
            txn,
            change,
            &groups,
            &mut visiting,
            &mut visited,
            &mut ordered,
        )?;
    }
    Ok(ordered)
}

fn external_inode_position<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    inode: Inode,
) -> Result<Position<Hash>, RepositoryError> {
    let position = txn
        .inode_position(inode)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("inode {} has no graph position", inode.get()),
        })?;
    if position.change.is_root() {
        return Err(RepositoryError::InvalidOperation {
            message: format!("inode {} resolves to the ROOT change", inode.get()),
        });
    }
    let change = txn
        .get_external(position.change)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "inode {} references change {} without an external hash",
                inode.get(),
                position.change.get()
            ),
        })?;
    Ok(Position::new(change, position.pos))
}

fn push_unique(ops: &mut Vec<DeferredTreeOp>, op: DeferredTreeOp) {
    if !ops.iter().any(|existing| {
        existing.change == op.change && existing.inode == op.inode && existing.action == op.action
    }) {
        ops.push(op);
    }
}

fn external_position(
    change_hash: Hash,
    position: Position<Option<Hash>>,
) -> Result<Position<Hash>, RepositoryError> {
    let change = match position.change {
        None => change_hash,
        Some(hash) if hash == Hash::NONE => {
            return Err(RepositoryError::InvalidOperation {
                message: "tree operation inode cannot reference ROOT".to_string(),
            });
        }
        Some(hash) => hash,
    };
    Ok(Position::new(change, position.pos))
}

fn current_path_for_position<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    position: Position<Hash>,
) -> Result<Option<String>, RepositoryError> {
    let internal_change = txn
        .get_internal(&position.change)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "tree operation references unknown change {}",
                position.change.to_base32()
            ),
        })?;
    let inode = txn
        .position_inode(Position::new(internal_change, position.pos))
        .map_err(|e| RepositoryError::Database(e.to_string()))?
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("tree operation position {} has no inode", position),
        })?;
    txn.get_path(inode)
        .map_err(|e| RepositoryError::Database(e.to_string()))
}

fn push_delete_for_path<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    change: Hash,
    path: &str,
    ops: &mut Vec<DeferredTreeOp>,
) -> Result<(), RepositoryError> {
    let Some(inode) = txn
        .get_inode(path)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(());
    };
    let position = external_inode_position(txn, inode)?;
    let op = DeferredTreeOp {
        change,
        inode: position,
        baseline_path: Some(path.to_string()),
        directory: Some(
            txn.is_directory(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?,
        ),
        action: DeferredTreeAction::Delete,
    };
    push_unique(ops, op);
    Ok(())
}

/// Collect graph-backed lifecycle metadata for the canonical tree projection.
/// Recording every add, delete, move, and undelete lets native record, import,
/// insert, and deferred replay derive the same cache state from visibility.
pub(super) fn collect_tree_ops<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    change_hash: Hash,
    change: &Change,
    deleted_paths: &[String],
) -> Result<Vec<DeferredTreeOp>, RepositoryError> {
    let mut ops = Vec::new();

    for graph_op in change.hunks() {
        match graph_op {
            GraphOp::FileAdd {
                add_inode, path, ..
            }
            | GraphOp::DirAdd {
                add_inode, path, ..
            } => {
                let added_position = Position::new(change_hash, add_inode.start);
                let directory = matches!(graph_op, GraphOp::DirAdd { .. });
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode: added_position,
                        baseline_path: None,
                        directory: Some(directory),
                        action: DeferredTreeAction::Set { path: path.clone() },
                    },
                );
            }
            GraphOp::FileMove { add, path, .. } => {
                let external_position = external_position(change_hash, add.inode)?;
                let op = DeferredTreeOp {
                    change: change_hash,
                    inode: external_position,
                    baseline_path: current_path_for_position(txn, external_position)?,
                    directory: Some(false),
                    action: DeferredTreeAction::Set { path: path.clone() },
                };
                // Keep the event even when TREE already contains `path`.
                // `atomic move` updates tracking before the subsequent record,
                // and that recorded change must still supersede an older
                // deferred rename for views where it is visible.
                push_unique(&mut ops, op);
            }
            GraphOp::FileDel { del, path, .. } | GraphOp::DirDel { del, path } => {
                let inode = external_position(change_hash, del.inode)?;
                let directory = matches!(graph_op, GraphOp::DirDel { .. });
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode,
                        baseline_path: Some(path.clone()),
                        directory: Some(directory),
                        action: DeferredTreeAction::Delete,
                    },
                );
            }
            GraphOp::FileUndel { undel, path, .. } | GraphOp::DirUndel { undel, path } => {
                let inode = external_position(change_hash, undel.inode)?;
                let directory = matches!(graph_op, GraphOp::DirUndel { .. });
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode,
                        baseline_path: current_path_for_position(txn, inode)?,
                        directory: Some(directory),
                        action: DeferredTreeAction::Set { path: path.clone() },
                    },
                );
            }
            GraphOp::Edit {
                change: atomic_core::change::Atom::EdgeUpdate(delete),
                local,
                ..
            }
            | GraphOp::Replacement {
                change: delete,
                local,
                ..
            } if deleted_paths.iter().any(|path| path == &local.path) => {
                let inode = external_position(change_hash, delete.inode)?;
                push_unique(
                    &mut ops,
                    DeferredTreeOp {
                        change: change_hash,
                        inode,
                        baseline_path: Some(local.path.clone()),
                        directory: Some(false),
                        action: DeferredTreeAction::Delete,
                    },
                );
            }
            _ => {}
        }
    }

    // Some import deletion paths are represented as content replacements,
    // not FileDel hunks, so preserve their TREE intent explicitly as well.
    for path in deleted_paths {
        let already_resolved = ops.iter().any(|op| {
            matches!(op.action, DeferredTreeAction::Delete)
                && op.baseline_path.as_deref() == Some(path.as_str())
        });
        if !already_resolved {
            push_delete_for_path(txn, change_hash, path, &mut ops)?;
        }
    }

    Ok(ops)
}

#[derive(Debug, Clone)]
pub(super) struct PreparedTreeProjection {
    ops: Vec<DeferredTreeOp>,
    prerequisites: TreeProjectionPlan,
}

impl PreparedTreeProjection {
    pub(super) fn apply_prerequisites<T: MutTxnT>(
        &self,
        txn: &mut T,
    ) -> Result<(), RepositoryError> {
        self.prerequisites.apply(txn).map_err(projection_error)
    }
}

impl Repository {
    pub(super) fn plan_tree_projection(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        change_id: NodeId,
        change_hash: Hash,
        change: &Change,
        deleted_paths: &[String],
        preserve_existing_tree_paths: bool,
    ) -> Result<PreparedTreeProjection, RepositoryError> {
        let ops = collect_tree_ops(&*txn, change_hash, change, deleted_paths)?;
        let mut additions = Vec::<(Position<NodeId>, String, TreeProjectionKind)>::new();
        for graph_op in change.hunks() {
            match graph_op {
                GraphOp::FileAdd {
                    add_inode, path, ..
                } => additions.push((
                    Position::new(change_id, add_inode.start),
                    path.clone(),
                    TreeProjectionKind::File,
                )),
                GraphOp::DirAdd {
                    add_inode, path, ..
                } => additions.push((
                    Position::new(change_id, add_inode.start),
                    path.clone(),
                    TreeProjectionKind::Directory,
                )),
                _ => {}
            }
        }
        let addition_positions: HashSet<Position<NodeId>> =
            additions.iter().map(|(position, _, _)| *position).collect();

        // Resolve every non-add inode before allocating or mutating derived
        // indexes. Missing change IDs and reverse inode rows fail closed here.
        for op in &ops {
            let internal_change = txn
                .get_internal(&op.inode.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection references unknown change {}",
                        op.inode.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, op.inode.pos);
            if addition_positions.contains(&position) {
                continue;
            }
            txn.position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("tree projection position {} has no inode", position),
                })?;
        }

        let mut projection_ops = Vec::new();
        for (position, path, kind) in additions {
            let inode = if let Some(existing) = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
            {
                existing
            } else if !preserve_existing_tree_paths {
                match txn
                    .get_inode(&path)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
                {
                    Some(staged)
                        if txn
                            .inode_position(staged)
                            .map_err(|error| RepositoryError::Database(error.to_string()))?
                            .is_none() =>
                    {
                        staged
                    }
                    Some(occupied) => {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "cannot add '{}': path is already bound to graph inode {}",
                                path,
                                occupied.get()
                            ),
                        });
                    }
                    None => txn
                        .alloc_inode()
                        .map_err(|error| RepositoryError::Database(error.to_string()))?,
                }
            } else {
                txn.alloc_inode()
                    .map_err(|error| RepositoryError::Database(error.to_string()))?
            };
            projection_ops.push(TreeProjectionOperation::Add {
                inode,
                path: None,
                position: Some(position),
                kind,
            });
        }

        for graph_op in change.hunks() {
            let (inode_position, kind) = match graph_op {
                GraphOp::FileUndel { undel, .. } => (
                    external_position(change_hash, undel.inode)?,
                    TreeProjectionKind::File,
                ),
                GraphOp::DirUndel { undel, .. } => (
                    external_position(change_hash, undel.inode)?,
                    TreeProjectionKind::Directory,
                ),
                _ => continue,
            };
            let internal_change = txn
                .get_internal(&inode_position.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "undelete references unknown change {}",
                        inode_position.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, inode_position.pos);
            let inode = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("undelete position {} has no inode", position),
                })?;
            projection_ops.push(TreeProjectionOperation::Add {
                inode,
                path: None,
                position: Some(position),
                kind,
            });
        }

        let prerequisites =
            TreeProjectionPlan::plan(&*txn, projection_ops).map_err(projection_error)?;
        Ok(PreparedTreeProjection { ops, prerequisites })
    }

    pub(super) fn apply_tree_projection(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        prepared: &PreparedTreeProjection,
        view_name: &str,
        preserve_existing_tree_paths: bool,
    ) -> Result<HashSet<String>, RepositoryError> {
        let (journal, changed) = self.merge_deferred_tree_ops(&prepared.ops)?;
        let view = txn
            .get_view(view_name)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility = graph_visibility_closure(&*txn, &view)?;
        self.validate_deferred_tree_metadata(&*txn, &journal, &visibility)?;

        let affected = if preserve_existing_tree_paths {
            HashSet::new()
        } else {
            self.apply_deferred_tree_ops_in_txn(txn, &journal, &visibility)?
        };
        if changed {
            self.persist_deferred_tree_journal(&journal)?;
        }
        Ok(affected)
    }

    fn deferred_tree_journal_path(&self) -> PathBuf {
        self.dot_dir.join(DEFERRED_TREE_JOURNAL)
    }

    fn load_deferred_tree_journal(&self) -> Result<DeferredTreeJournal, RepositoryError> {
        let path = self.deferred_tree_journal_path();
        if !path.is_file() {
            return Ok(DeferredTreeJournal::default());
        }
        let journal: DeferredTreeJournal = serde_json::from_slice(&std::fs::read(path)?)?;
        if journal.version != DEFERRED_TREE_JOURNAL_VERSION {
            return Err(RepositoryError::Serialization(format!(
                "unsupported deferred TREE journal version {}",
                journal.version
            )));
        }
        Ok(journal)
    }

    /// Project the path/lifecycle state for a change closure without treating
    /// the global TREE cache or rendered byte length as file presence.
    pub(super) fn project_tree_for_visibility<T>(
        &self,
        txn: &T,
        visibility: &GraphVisibilityClosure,
    ) -> Result<TreeProjection, RepositoryError>
    where
        T: GraphTxnT + TreeTxnT,
    {
        let journal = self.load_deferred_tree_journal()?;
        let mut visible_hashes = HashSet::with_capacity(visibility.len());
        for change_id in visibility.iter_dependency_first().copied() {
            let hash = txn
                .get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "validated visible change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            visible_hashes.insert(hash);
        }
        let ordered_ops = causally_order_tree_ops(txn, &journal.ops)?;
        let desired = desired_tree_paths_for_txn(txn, &ordered_ops, &visible_hashes)?;

        let mut current = HashMap::new();
        for entry in txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let (path, inode) = entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
            let Some(position) = txn
                .inode_position(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            else {
                continue;
            };
            if position.change.is_root() {
                continue;
            }
            let Some(change) = txn
                .get_external(position.change)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            else {
                continue;
            };
            current.insert(
                Position::new(change, position.pos),
                (
                    inode,
                    path,
                    txn.is_directory(inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?,
                    position,
                ),
            );
        }

        let mut positions: HashSet<Position<Hash>> = current.keys().copied().collect();
        positions.extend(desired.keys().copied());

        let mut projection = TreeProjection::default();
        let mut absent_by_path = HashMap::new();
        for external_position in positions {
            let resolved = if let Some((inode, path, is_directory, position)) =
                current.get(&external_position)
            {
                Some((*inode, Some(path.clone()), *is_directory, *position))
            } else {
                let internal_change = txn
                    .get_internal(&external_position.change)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection references unknown change {}",
                            external_position.change.to_base32()
                        ),
                    })?;
                let position = Position::new(internal_change, external_position.pos);
                let inode = txn
                    .position_inode(position)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::InvalidOperation {
                        message: format!("tree projection position {} has no inode", position),
                    })?;
                let cached_directory = txn
                    .is_directory(inode)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let is_directory = desired
                    .get(&external_position)
                    .and_then(|state| state.directory)
                    .unwrap_or(cached_directory);
                if is_directory != cached_directory {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection kind for inode {} disagrees with DIRECTORIES",
                            inode.get()
                        ),
                    });
                }
                Some((
                    inode,
                    txn.get_path(inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?,
                    is_directory,
                    position,
                ))
            };
            let Some((inode, current_path, is_directory, position)) = resolved else {
                continue;
            };
            if let Some(expected) = desired
                .get(&external_position)
                .and_then(|state| state.directory)
            {
                if expected != is_directory {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection kind for inode {} disagrees with DIRECTORIES",
                            inode.get()
                        ),
                    });
                }
            }

            if !visibility.contains(position.change) {
                continue;
            }

            match desired.get(&external_position) {
                None => {
                    if let Some(path) = current_path {
                        let item = if is_directory {
                            OutputItem::directory(path.clone(), inode)
                        } else {
                            OutputItem::file(path.clone(), inode, position)
                        };
                        projection.present.insert(path, item);
                    }
                }
                Some(state) => {
                    let mut projected_path = if state.ambiguous {
                        current_path
                            .clone()
                            .or_else(|| state.last_present_path.clone())
                    } else {
                        state.desired_path.clone()
                    };
                    if projected_path.is_none()
                        && state.deleted
                        && !is_directory
                        && crate::repository::status::is_file_alive_via_retrieval(
                            txn, inode, position, visibility,
                        )?
                    {
                        projected_path = state
                            .last_present_path
                            .clone()
                            .or_else(|| current_path.clone());
                    }

                    if let Some(path) = projected_path {
                        let item = if is_directory {
                            OutputItem::directory(path.clone(), inode)
                        } else {
                            OutputItem::file(path.clone(), inode, position)
                        };
                        projection.present.insert(path, item);
                    } else if !is_directory {
                        for path in &state.known_paths {
                            absent_by_path.insert(
                                path.clone(),
                                MaterializedEntry::absent(path.clone(), Some(inode)),
                            );
                        }
                        if let Some(path) = current_path {
                            absent_by_path
                                .insert(path.clone(), MaterializedEntry::absent(path, Some(inode)));
                        }
                    }
                }
            }
        }

        absent_by_path.retain(|path, _| !projection.present.contains_key(path));
        projection.absent = absent_by_path.into_values().collect();
        projection
            .absent
            .sort_by(|left, right| left.path().cmp(right.path()));
        Ok(projection)
    }

    fn merge_deferred_tree_ops(
        &self,
        ops: &[DeferredTreeOp],
    ) -> Result<(DeferredTreeJournal, bool), RepositoryError> {
        let mut journal = self.load_deferred_tree_journal()?;
        let mut changed = false;

        // Every graph-backed lifecycle operation participates. Sparse,
        // view-order-dependent participation made TREE state depend on which
        // view happened to be active when an operation arrived.
        for op in ops {
            if let Some(existing) = journal.ops.iter_mut().find(|existing| {
                existing.change == op.change
                    && existing.inode == op.inode
                    && existing.action == op.action
            }) {
                if existing.baseline_path.is_none() {
                    existing.baseline_path.clone_from(&op.baseline_path);
                    changed = true;
                }
                if existing.directory.is_none() {
                    existing.directory = op.directory;
                    changed = true;
                }
            } else {
                journal.ops.push(op.clone());
                changed = true;
            }
        }
        Ok((journal, changed))
    }

    fn persist_deferred_tree_journal(
        &self,
        journal: &DeferredTreeJournal,
    ) -> Result<(), RepositoryError> {
        let path = self.deferred_tree_journal_path();
        let mut temp = tempfile::NamedTempFile::new_in(&self.dot_dir)?;
        serde_json::to_writer_pretty(temp.as_file_mut(), journal)?;
        temp.as_file_mut().write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| {
            RepositoryError::Io(std::io::Error::other(format!(
                "failed to persist deferred TREE journal: {}",
                error
            )))
        })?;
        self.sync_dot_dir()
    }

    fn deferred_tree_alignment_pending_path(&self) -> PathBuf {
        self.dot_dir.join(DEFERRED_TREE_ALIGNMENT_PENDING)
    }

    fn write_deferred_tree_alignment_pending(
        &self,
        source_view: &str,
        target_view: &str,
    ) -> Result<(), RepositoryError> {
        let pending = PendingTreeAlignment {
            version: DEFERRED_TREE_JOURNAL_VERSION,
            source_view: source_view.to_string(),
            target_view: target_view.to_string(),
        };
        let path = self.deferred_tree_alignment_pending_path();
        let mut temp = tempfile::NamedTempFile::new_in(&self.dot_dir)?;
        serde_json::to_writer(temp.as_file_mut(), &pending)?;
        temp.as_file_mut().write_all(b"\n")?;
        temp.as_file().sync_all()?;
        temp.persist(&path).map_err(|error| {
            RepositoryError::Io(std::io::Error::other(format!(
                "failed to persist deferred TREE alignment marker: {}",
                error
            )))
        })?;
        self.sync_dot_dir()?;
        Ok(())
    }

    fn load_deferred_tree_alignment_pending(
        &self,
    ) -> Result<PendingTreeAlignment, RepositoryError> {
        let pending: PendingTreeAlignment =
            serde_json::from_slice(&std::fs::read(self.deferred_tree_alignment_pending_path())?)?;
        if pending.version != DEFERRED_TREE_JOURNAL_VERSION {
            return Err(RepositoryError::Serialization(format!(
                "unsupported deferred TREE alignment version {}",
                pending.version
            )));
        }
        Ok(pending)
    }

    fn lock_deferred_tree_alignment(&self) -> Result<std::fs::File, RepositoryError> {
        use fs2::FileExt;

        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dot_dir.join(DEFERRED_TREE_ALIGNMENT_LOCK))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    fn clear_deferred_tree_alignment_pending(&self) -> Result<(), RepositoryError> {
        match std::fs::remove_file(self.deferred_tree_alignment_pending_path()) {
            Ok(()) => self.sync_dot_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RepositoryError::Io(error)),
        }
    }

    pub(super) fn has_pending_deferred_tree_alignment(&self) -> bool {
        self.deferred_tree_alignment_pending_path().is_file()
    }

    fn validate_deferred_tree_metadata<T: GraphTxnT + TreeTxnT>(
        &self,
        txn: &T,
        journal: &DeferredTreeJournal,
        visibility: &GraphVisibilityClosure,
    ) -> Result<(), RepositoryError> {
        let mut visible_hashes = HashSet::with_capacity(visibility.len());
        for change_id in visibility.iter_dependency_first().copied() {
            let hash = txn
                .get_external(change_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "validated visible change {} has no external hash",
                        change_id.get()
                    ),
                })?;
            visible_hashes.insert(hash);
        }
        let ordered_ops = causally_order_tree_ops(txn, &journal.ops)?;
        let desired = desired_tree_paths_for_txn(txn, &ordered_ops, &visible_hashes)?;
        for (external_position, state) in desired {
            let internal_change = txn
                .get_internal(&external_position.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection references unknown change {}",
                        external_position.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, external_position.pos);
            let inode = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("tree projection position {} has no inode", position),
                })?;
            if let Some(expected_directory) = state.directory {
                let cached_directory = txn
                    .is_directory(inode)
                    .map_err(|error| RepositoryError::Database(error.to_string()))?;
                if expected_directory != cached_directory {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "tree projection kind for inode {} disagrees with DIRECTORIES",
                            inode.get()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    fn apply_deferred_tree_ops_in_txn(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        journal: &DeferredTreeJournal,
        visibility: &GraphVisibilityClosure,
    ) -> Result<HashSet<String>, RepositoryError> {
        let mut visible_hashes = HashSet::with_capacity(visibility.len());
        for change_id in visibility.iter_dependency_first().copied() {
            let hash = txn
                .get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "validated visible change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            visible_hashes.insert(hash);
        }

        let ordered_ops = causally_order_tree_ops(&*txn, &journal.ops)?;
        let desired = desired_tree_paths_for_txn(&*txn, &ordered_ops, &visible_hashes)?;
        let mut operations = Vec::new();
        let mut affected_paths = HashSet::new();
        let mut final_paths = HashMap::<String, Inode>::new();
        for entry in txn
            .iter_tree()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            let (path, inode) =
                entry.map_err(|error| RepositoryError::Database(error.to_string()))?;
            final_paths.insert(path, inode);
        }
        let mut projected_directories = Vec::<(String, Inode)>::new();
        let mut path_updates = Vec::<(Inode, Option<String>, Option<String>)>::new();

        for (external_position, state) in desired {
            let internal_change = txn
                .get_internal(&external_position.change)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection references unknown change {}",
                        external_position.change.to_base32()
                    ),
                })?;
            let position = Position::new(internal_change, external_position.pos);
            let inode = txn
                .position_inode(position)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("tree projection position {} has no inode", position),
                })?;
            let cached_directory = txn
                .is_directory(inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let is_directory = state.directory.unwrap_or(cached_directory);
            if is_directory != cached_directory {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "tree projection kind for inode {} disagrees with DIRECTORIES",
                        inode.get()
                    ),
                });
            }

            let current_path = txn
                .get_path(inode)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let mut desired_path = if !visibility.contains(internal_change) {
                None
            } else if state.ambiguous {
                current_path
                    .clone()
                    .or_else(|| state.last_present_path.clone())
            } else {
                state.desired_path.clone()
            };
            if desired_path.is_none()
                && state.deleted
                && !is_directory
                && visibility.contains(internal_change)
                && crate::repository::status::is_file_alive_via_retrieval(
                    &*txn, inode, position, visibility,
                )?
            {
                desired_path = state
                    .last_present_path
                    .clone()
                    .or_else(|| current_path.clone());
            }

            path_updates.push((inode, current_path.clone(), desired_path.clone()));
            if current_path != desired_path {
                if let Some(path) = &current_path {
                    affected_paths.insert(path.clone());
                }
                if let Some(path) = &desired_path {
                    affected_paths.insert(path.clone());
                }
            }
            match desired_path.clone() {
                Some(path) if current_path.as_deref() == Some(path.as_str()) => {
                    operations.push(TreeProjectionOperation::Undelete {
                        inode,
                        path,
                        kind: if is_directory {
                            TreeProjectionKind::Directory
                        } else {
                            TreeProjectionKind::File
                        },
                    });
                }
                Some(path) if current_path.is_some() => {
                    operations.push(TreeProjectionOperation::Move { inode, path });
                }
                Some(path) => operations.push(TreeProjectionOperation::Undelete {
                    inode,
                    path,
                    kind: if is_directory {
                        TreeProjectionKind::Directory
                    } else {
                        TreeProjectionKind::File
                    },
                }),
                None if current_path.is_some() => {
                    operations.push(TreeProjectionOperation::Delete {
                        inode,
                        retire: false,
                    })
                }
                None => {}
            }
            if is_directory {
                if let Some(path) = desired_path {
                    projected_directories.push((path, inode));
                }
            }
        }

        for (inode, current_path, _) in &path_updates {
            if let Some(path) = current_path {
                if final_paths.get(path) == Some(inode) {
                    final_paths.remove(path);
                }
            }
        }
        for (inode, _, desired_path) in &path_updates {
            if let Some(path) = desired_path {
                final_paths.insert(path.clone(), *inode);
            }
        }

        // DIR_EMPTY is a projection of exact direct-child membership after all
        // visible add/delete/move/undelete effects, never of path prefixes or
        // the previous DIR_EMPTY value.
        for (path, inode) in projected_directories {
            let empty = !final_paths
                .keys()
                .any(|candidate| projected_parent(candidate) == Some(path.as_str()));
            operations.push(TreeProjectionOperation::DirectoryOccupancy { inode, empty });
        }

        TreeProjectionPlan::plan(&*txn, operations)
            .map_err(projection_error)?
            .apply(txn)
            .map_err(projection_error)?;
        Ok(affected_paths)
    }

    /// Recover a switch interrupted between TREE alignment and publishing the
    /// current-view pointer. The advisory lock is released by the OS on process
    /// exit, so a concurrent opener waits for a live switch and only performs
    /// recovery when the marker survives that lock handoff.
    pub(super) fn recover_pending_deferred_tree_alignment(
        &mut self,
    ) -> Result<(), RepositoryError> {
        if self.is_sandbox || !self.has_pending_deferred_tree_alignment() {
            return Ok(());
        }

        let _alignment_lock = self.lock_deferred_tree_alignment()?;
        if !self.has_pending_deferred_tree_alignment() {
            return Ok(());
        }
        let pending = self.load_deferred_tree_alignment_pending()?;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let journal = self.load_deferred_tree_journal()?;
        let source_view = txn
            .get_view(&pending.source_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: pending.source_view.clone(),
            })?;
        let visibility = graph_visibility_closure(&txn, &source_view)?;
        self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, &visibility)?;
        self.write_current_view(&pending.source_view)?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        self.current_view = pending.source_view;
        self.clear_deferred_tree_alignment_pending()?;
        Ok(())
    }

    /// Align TREE and publish the view pointer as one recoverable transition.
    /// The database write lock is held while the pointer is written, and the
    /// pending marker lets the next writable open reconcile either side after
    /// a process crash.
    pub(super) fn align_deferred_tree_and_publish_view(
        &mut self,
        view_name: &str,
        visibility: &GraphVisibilityClosure,
    ) -> Result<HashSet<String>, RepositoryError> {
        let _alignment_lock = self.lock_deferred_tree_alignment()?;
        // `current_view` can intentionally be scoped to a background target
        // via set_current_view_in_memory(). Read the persisted pointer only
        // after taking the alignment lock: it owns the materialized TREE and
        // is therefore the only valid rollback source for this transition.
        let old_view = Self::read_current_view(&self.dot_dir)?;
        let mut txn = match self.pristine.write_txn() {
            Ok(txn) => txn,
            Err(error) => return Err(RepositoryError::Database(error.to_string())),
        };
        let journal = self.load_deferred_tree_journal()?;
        // Publish the marker only after taking the database write lock. Any
        // opener that observes it must wait for this transaction. If the
        // marker survives, recovery restores the source view.
        self.write_deferred_tree_alignment_pending(&old_view, view_name)?;
        let affected_paths =
            match self.apply_deferred_tree_ops_in_txn(&mut txn, &journal, visibility) {
                Ok(paths) => paths,
                Err(error) => {
                    let _ = self.clear_deferred_tree_alignment_pending();
                    return Err(error);
                }
            };

        if let Err(error) = self.write_current_view(view_name) {
            let _ = self.clear_deferred_tree_alignment_pending();
            return Err(error);
        }

        if let Err(error) = txn.commit() {
            // The DB transaction did not publish. Restore the pointer while
            // retaining the marker if restoration itself fails, so the next
            // writable open can reconcile from the persisted pointer.
            if self.write_current_view(&old_view).is_ok() {
                let _ = self.clear_deferred_tree_alignment_pending();
            }
            return Err(RepositoryError::Database(error.to_string()));
        }

        self.current_view = view_name.to_string();
        // Clearing the marker is the commit point for this recoverable
        // transition. Propagate cleanup or directory-sync failures so a switch
        // is never reported durable while recovery may still roll it back.
        self.clear_deferred_tree_alignment_pending()?;
        Ok(affected_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::types::ChangePosition;

    fn hash(label: &str) -> Hash {
        Hash::of(label.as_bytes())
    }

    fn set(
        change: Hash,
        inode: Position<Hash>,
        baseline: Option<&str>,
        path: &str,
    ) -> DeferredTreeOp {
        DeferredTreeOp {
            change,
            inode,
            baseline_path: baseline.map(str::to_string),
            directory: Some(false),
            action: DeferredTreeAction::Set {
                path: path.to_string(),
            },
        }
    }

    #[test]
    fn planner_handles_causal_rename_chains_and_view_visibility() {
        let inode = Position::new(hash("creator"), ChangePosition::new(7));
        let first = hash("move-a-b");
        let second = hash("move-b-c");
        let ops = vec![
            set(first, inode, Some("a.txt"), "b.txt"),
            set(second, inode, Some("stale-baseline.txt"), "c.txt"),
        ];

        let base = desired_tree_paths(&ops, &HashSet::new(), |_, _| Ok(false)).unwrap();
        assert_eq!(base[&inode].desired_path.as_deref(), Some("a.txt"));

        let visible = HashSet::from([first, second]);
        let moved = desired_tree_paths(&ops, &visible, |descendant, ancestor| {
            Ok(descendant == second && ancestor == first)
        })
        .unwrap();
        assert_eq!(moved[&inode].desired_path.as_deref(), Some("c.txt"));
    }

    #[test]
    fn planner_applies_causally_later_visible_deletion() {
        let inode = Position::new(hash("creator"), ChangePosition::new(9));
        let moved = hash("move");
        let deleted = hash("delete");
        let ops = vec![
            set(moved, inode, Some("old.txt"), "new.txt"),
            DeferredTreeOp {
                change: deleted,
                inode,
                baseline_path: Some("new.txt".into()),
                directory: Some(false),
                action: DeferredTreeAction::Delete,
            },
        ];

        let desired = desired_tree_paths(
            &ops,
            &HashSet::from([moved, deleted]),
            |descendant, ancestor| Ok(descendant == deleted && ancestor == moved),
        )
        .unwrap();
        assert_eq!(desired[&inode].desired_path, None);
    }

    #[test]
    fn planner_projects_causally_later_undelete() {
        let inode = Position::new(hash("creator"), ChangePosition::new(10));
        let deleted = hash("delete");
        let restored = hash("undelete");
        let ops = vec![
            DeferredTreeOp {
                change: deleted,
                inode,
                baseline_path: Some("file.txt".into()),
                directory: Some(false),
                action: DeferredTreeAction::Delete,
            },
            set(restored, inode, None, "file.txt"),
        ];

        let desired = desired_tree_paths(
            &ops,
            &HashSet::from([deleted, restored]),
            |descendant, ancestor| Ok(descendant == restored && ancestor == deleted),
        )
        .unwrap();
        assert_eq!(desired[&inode].desired_path.as_deref(), Some("file.txt"));
        assert!(!desired[&inode].deleted);
    }

    #[test]
    fn planner_does_not_use_journal_order_for_concurrent_renames() {
        let inode = Position::new(hash("creator"), ChangePosition::new(11));
        let left = hash("left-rename");
        let right = hash("right-rename");
        let ops = vec![
            set(left, inode, Some("base.txt"), "left.txt"),
            set(right, inode, Some("base.txt"), "right.txt"),
        ];

        let desired =
            desired_tree_paths(&ops, &HashSet::from([left, right]), |_, _| Ok(false)).unwrap();
        assert!(desired[&inode].ambiguous);
        assert_eq!(desired[&inode].desired_path, None);
        assert_eq!(
            desired[&inode].last_present_path.as_deref(),
            Some("base.txt")
        );
    }

    #[test]
    fn planner_preserves_concurrent_same_path_claims() {
        let left_inode = Position::new(hash("left-creator"), ChangePosition::new(3));
        let right_inode = Position::new(hash("right-creator"), ChangePosition::new(5));
        let left = hash("left-add");
        let right = hash("right-add");
        let ops = vec![
            set(left, left_inode, None, "same.txt"),
            set(right, right_inode, None, "same.txt"),
        ];

        let desired =
            desired_tree_paths(&ops, &HashSet::from([left, right]), |_, _| Ok(false)).unwrap();
        assert_eq!(
            desired[&left_inode].desired_path.as_deref(),
            Some("same.txt")
        );
        assert_eq!(
            desired[&right_inode].desired_path.as_deref(),
            Some("same.txt")
        );
    }
}

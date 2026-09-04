//! Verification and atomic repair of native derived repository indexes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use atomic_core::change::GraphOp;
use atomic_core::pristine::{
    directory_flags, GraphTxnT, GraphVisibilityClosure, InodeGraphOps, NativeDerivedIndexes,
    NativeDerivedIndexesMutTxnT, PathClaimEntry, PathClaimKind, PathClaimTxnT, PristineError,
    StoredConflict, StoredConflictKind, TreeTxnT, ViewTxnT,
};
use atomic_core::types::{Inode, NodeId, Position};

use super::*;

/// Native derived table audited by `atomic doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NativeIndex {
    PathClaims,
    Tree,
    RevTree,
    Inodes,
    RevInodes,
    Directories,
    Conflicts,
}

impl NativeIndex {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PathClaims => "PATH_CLAIMS",
            Self::Tree => "TREE",
            Self::RevTree => "REV_TREE",
            Self::Inodes => "INODES",
            Self::RevInodes => "REV_INODES",
            Self::Directories => "DIRECTORIES",
            Self::Conflicts => "CONFLICTS",
        }
    }
}

impl std::fmt::Display for NativeIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kind of mismatch between an authoritative projection and its stored cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NativeIndexProblemKind {
    Missing,
    Stale,
    Mismatched,
    Malformed,
    Unrepairable,
}

impl std::fmt::Display for NativeIndexProblemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Missing => "missing",
            Self::Stale => "stale",
            Self::Mismatched => "mismatched",
            Self::Malformed => "malformed",
            Self::Unrepairable => "unrepairable",
        };
        f.write_str(value)
    }
}

/// One deterministic native-index diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeIndexProblem {
    pub index: NativeIndex,
    pub kind: NativeIndexProblemKind,
    pub key: String,
    pub expected: Option<String>,
    pub actual: Option<String>,
}

impl std::fmt::Display for NativeIndexProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} row '{}'", self.index, self.kind, self.key)?;
        if let Some(expected) = &self.expected {
            write!(f, " expected={expected}")?;
        }
        if let Some(actual) = &self.actual {
            write!(f, " actual={actual}")?;
        }
        Ok(())
    }
}

/// Read-only comparison of native derived indexes with graph/change authority.
#[derive(Debug, Clone, Default)]
pub struct NativeIndexReport {
    pub expected_rows: usize,
    pub actual_rows: usize,
    pub problems: Vec<NativeIndexProblem>,
}

impl NativeIndexReport {
    pub fn is_healthy(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Outcome of an all-or-nothing native-index repair.
#[derive(Debug, Clone, Default)]
pub struct NativeIndexRepairOutcome {
    pub problems_repaired: usize,
    pub rows_written: usize,
    pub already_healthy: bool,
}

impl Repository {
    /// Compare every native derived table with a deterministic graph projection.
    pub fn verify_native_derived_indexes(&self) -> Result<NativeIndexReport, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let expected = match self.build_native_derived_indexes(&txn) {
            Ok(expected) => expected,
            Err(error) => {
                return Ok(NativeIndexReport {
                    problems: vec![NativeIndexProblem {
                        index: NativeIndex::Inodes,
                        kind: NativeIndexProblemKind::Unrepairable,
                        key: "<projection>".to_string(),
                        expected: None,
                        actual: Some(error.to_string()),
                    }],
                    ..NativeIndexReport::default()
                });
            }
        };
        compare_native_derived_indexes(&txn, &expected)
    }

    /// Atomically rebuild every native derived table from graph and change facts.
    pub fn repair_native_derived_indexes(
        &self,
    ) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        self.repair_native_derived_indexes_inner(false)
    }

    fn repair_native_derived_indexes_inner(
        &self,
        fail_before_commit: bool,
    ) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn_immediate()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let expected = self.build_native_derived_indexes(&txn)?;
        let before = compare_native_derived_indexes(&txn, &expected)?;
        if before.is_healthy() {
            txn.abort()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            return Ok(NativeIndexRepairOutcome {
                already_healthy: true,
                rows_written: native_row_count(&expected),
                ..NativeIndexRepairOutcome::default()
            });
        }

        txn.replace_native_derived_indexes(&expected)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let after = compare_native_derived_indexes(&txn, &expected)?;
        if !after.is_healthy() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "native-index replacement failed post-write verification with {} problem(s)",
                    after.problems.len()
                ),
            });
        }
        if fail_before_commit {
            return Err(RepositoryError::InvalidOperation {
                message: "injected native-index repair failure before commit".to_string(),
            });
        }

        txn.commit()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(NativeIndexRepairOutcome {
            problems_repaired: before.problems.len(),
            rows_written: native_row_count(&expected),
            already_healthy: false,
        })
    }

    #[cfg(test)]
    pub(super) fn repair_native_derived_indexes_with_injected_failure(
        &self,
    ) -> Result<NativeIndexRepairOutcome, RepositoryError> {
        self.repair_native_derived_indexes_inner(true)
    }

    fn build_native_derived_indexes<T>(
        &self,
        txn: &T,
    ) -> Result<NativeDerivedIndexes, RepositoryError>
    where
        T: GraphTxnT
            + TreeTxnT
            + PathClaimTxnT
            + ViewTxnT
            + InodeGraphOps<InodeError = PristineError>,
    {
        let view_snapshot = txn
            .snapshot_views()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let view_names: Vec<String> = view_snapshot.iter().map(|(name, _)| name.clone()).collect();

        let mut reachable = Vec::new();
        let mut reachable_set = HashSet::new();
        for view_name in &view_names {
            let view = txn
                .get_view(view_name)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.clone(),
                })?;
            let visibility = graph_visibility_closure(txn, &view)?;
            for change_id in visibility.iter_dependency_first().copied() {
                if !change_id.is_root() && reachable_set.insert(change_id) {
                    reachable.push(change_id);
                }
            }
        }

        let mut ordered = Vec::with_capacity(reachable.len());
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        for change_id in reachable {
            visit_reachable_change(
                txn,
                change_id,
                &reachable_set,
                &mut visiting,
                &mut visited,
                &mut ordered,
            )?;
        }

        let mut changes = Vec::with_capacity(ordered.len());
        let mut root_kinds = BTreeMap::<Position<NodeId>, PathClaimKind>::new();
        for change_id in ordered {
            let hash = txn
                .get_external(change_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "reachable change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            let change = self.change_store.load_change(&hash).map_err(|error| {
                RepositoryError::Database(format!(
                    "cannot load reachable change {} while rebuilding native indexes: {}",
                    hash, error
                ))
            })?;
            for operation in change.hunks() {
                let root = match operation {
                    GraphOp::FileAdd { add_inode, .. } => Some((
                        Position::new(change_id, add_inode.start),
                        PathClaimKind::File,
                    )),
                    GraphOp::DirAdd { add_inode, .. } => Some((
                        Position::new(change_id, add_inode.start),
                        PathClaimKind::Directory,
                    )),
                    _ => None,
                };
                if let Some((position, kind)) = root {
                    if let Some(previous) = root_kinds.insert(position, kind) {
                        if previous != kind {
                            return Err(RepositoryError::InvalidOperation {
                                message: format!(
                                    "graph root {} changes between file and directory",
                                    position
                                ),
                            });
                        }
                    }
                }
            }
            changes.push((change_id, change));
        }

        let inode_graph_keys = txn
            .snapshot_inode_graph_keys()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let existing_rev_inodes: BTreeMap<_, _> = txn
            .snapshot_rev_inodes()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .collect();
        let mut recovered = HashMap::<Position<NodeId>, Inode>::new();
        for position in root_kinds.keys().copied() {
            let candidates: BTreeSet<Inode> = inode_graph_keys
                .iter()
                .filter(|(_, node)| {
                    node.change == position.change
                        && node.start == position.pos
                        && node.end == position.pos
                })
                .map(|(inode, _)| *inode)
                .collect();
            let inode = match candidates.len() {
                1 => *candidates.iter().next().expect("one inode owner"),
                _ => match existing_rev_inodes.get(&position).copied() {
                    Some(existing) if candidates.contains(&existing) => existing,
                    _ => {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "graph root {} has {} candidate INODE_GRAPH owners and no matching existing binding; repair is unsafe",
                                position,
                                candidates.len()
                            ),
                        });
                    }
                },
            };
            let root = inode_graph_keys
                .iter()
                .find(|(candidate, node)| {
                    *candidate == inode
                        && node.change == position.change
                        && node.start == position.pos
                        && node.end == position.pos
                })
                .map(|(_, node)| *node)
                .expect("candidate was derived from an exact empty root");
            if !txn
                .has_vertex(root)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
            {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "INODE_GRAPH owner {} for root {} has no canonical GRAPH vertex",
                        inode.get(),
                        position
                    ),
                });
            }
            if existing_rev_inodes
                .get(&position)
                .is_some_and(|existing| *existing != inode)
            {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graph root {} is owned by inode {} in INODE_GRAPH but inode {} in REV_INODES",
                        position,
                        inode.get(),
                        existing_rev_inodes[&position].get()
                    ),
                });
            }
            recovered.insert(position, inode);
        }

        let mut claims = Vec::<PathClaimEntry>::new();
        for (change_id, change) in &changes {
            let mut change_claims =
                super::name_resolution::path_claim_events_for_change_with_prior_and_inodes(
                    txn, *change_id, change, &claims, &recovered,
                )?;
            claims.append(&mut change_claims);
        }
        claims.sort_by(|left, right| (&left.path, left.event).cmp(&(&right.path, right.event)));
        claims.dedup_by(|left, right| left.path == right.path && left.event == right.event);

        for claim in &claims {
            if !recovered.contains_key(&claim.event.claim.claimant) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "path claim '{}' references graph root {} with no unique inode owner",
                        claim.path, claim.event.claim.claimant
                    ),
                });
            }
        }

        let mut expected = NativeDerivedIndexes {
            path_claims: claims.clone(),
            ..NativeDerivedIndexes::default()
        };
        let mut inodes: Vec<_> = recovered
            .iter()
            .map(|(position, inode)| (*inode, *position))
            .collect();
        inodes.sort_by_key(|(inode, position)| (inode.get(), *position));
        inodes.dedup();
        let mut inode_owners = BTreeMap::<Inode, Position<NodeId>>::new();
        for (inode, position) in &inodes {
            if let Some(previous) = inode_owners.insert(*inode, *position) {
                if previous != *position {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "inode {} owns both graph roots {} and {}",
                            inode.get(),
                            previous,
                            position
                        ),
                    });
                }
            }
        }
        expected.inodes = inodes;
        expected.rev_inodes = expected
            .inodes
            .iter()
            .map(|(inode, position)| (*position, *inode))
            .collect();
        expected.rev_inodes.sort();

        let current_view = txn
            .get_view(&self.current_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;
        let mut current_reduced = None;
        let mut conflicts = BTreeMap::<(u64, Inode), Vec<StoredConflict>>::new();

        for view_name in view_names {
            let view = txn
                .get_view(&view_name)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.clone(),
                })?;
            let full_visibility = graph_visibility_closure(txn, &view)?;
            let claim_visibility =
                super::name_resolution::path_claim_visibility_for_view_with_entries(
                    txn,
                    &self.change_store,
                    &view,
                    &full_visibility,
                    &claims,
                )?;
            let reduced = super::name_resolution::reduce_path_claim_entries_with_inodes(
                txn,
                &claim_visibility,
                &claims,
                &recovered,
            )?;

            collect_expected_conflicts(
                txn,
                &self.change_store,
                view.id,
                &claim_visibility,
                &reduced,
                &mut conflicts,
            )?;
            if view.id == current_view.id {
                current_reduced = Some(reduced);
            }
        }

        let current_reduced = current_reduced.ok_or_else(|| RepositoryError::ViewNotFound {
            name: self.current_view.clone(),
        })?;
        let mut tree = BTreeMap::<String, Inode>::new();
        let mut reverse = BTreeMap::<Inode, String>::new();
        for side in &current_reduced.present {
            insert_tree_pair(&mut tree, &mut reverse, &side.path, side.inode)?;
        }

        let actual_tree = txn
            .iter_tree()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let actual_reverse: BTreeMap<_, _> = txn
            .iter_rev_tree_pairs()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .collect();
        let actual_inodes: BTreeSet<_> = txn
            .snapshot_inodes()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .map(|(inode, _)| inode)
            .collect();
        let actual_directories: BTreeMap<_, _> = txn
            .snapshot_directories()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .collect();
        let recorded_inodes: BTreeSet<_> =
            expected.inodes.iter().map(|(inode, _)| *inode).collect();
        let mut staged_directories = BTreeSet::new();
        let mut staged_paths = BTreeSet::new();
        for (path, inode) in actual_tree {
            if recorded_inodes.contains(&inode) {
                continue;
            }
            if actual_inodes.contains(&inode) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged path '{}' for inode {} has an unexpected INODES binding",
                        path,
                        inode.get()
                    ),
                });
            }
            if actual_reverse.get(&inode).map(String::as_str) != Some(path.as_str()) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged TREE row '{}' for inode {} is not bijective",
                        path,
                        inode.get()
                    ),
                });
            }
            insert_tree_pair(&mut tree, &mut reverse, &path, inode)?;
            staged_paths.insert(path.clone());
            if let Some(flags) = actual_directories.get(&inode) {
                let allowed = directory_flags::DIR_EXPLICIT | directory_flags::DIR_EMPTY;
                if flags & !allowed != 0 {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "graphless staged directory inode {} has invalid flags {flags:#04x}",
                            inode.get()
                        ),
                    });
                }
                staged_directories.insert(inode);
            }
        }
        for (inode, path) in &actual_reverse {
            if recorded_inodes.contains(inode) {
                continue;
            }
            if actual_inodes.contains(inode) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged reverse path '{}' for inode {} has an unexpected INODES binding",
                        path,
                        inode.get()
                    ),
                });
            }
            if tree.get(path) != Some(inode) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "graphless staged REV_TREE row for inode {} and '{}' is not bijective",
                        inode.get(),
                        path
                    ),
                });
            }
        }

        expected.tree = tree
            .iter()
            .map(|(path, inode)| (path.clone(), *inode))
            .collect();
        expected.rev_tree = reverse
            .iter()
            .map(|(inode, path)| (*inode, path.clone()))
            .collect();

        let mut alive_paths = BTreeSet::new();
        let mut directory_paths = BTreeMap::<Inode, BTreeSet<String>>::new();
        for side in &current_reduced.present {
            alive_paths.insert(side.path.clone());
            if side.kind == PathClaimKind::Directory {
                directory_paths
                    .entry(side.inode)
                    .or_default()
                    .insert(side.path.clone());
            }
        }
        for conflict in current_reduced.conflicts.values() {
            for side in &conflict.sides {
                alive_paths.insert(side.path.clone());
                if side.kind == PathClaimKind::Directory {
                    directory_paths
                        .entry(side.inode)
                        .or_default()
                        .insert(side.path.clone());
                }
            }
        }
        alive_paths.extend(staged_paths);
        for inode in staged_directories {
            if let Some(path) = reverse.get(&inode) {
                alive_paths.insert(path.clone());
                directory_paths
                    .entry(inode)
                    .or_default()
                    .insert(path.clone());
            }
        }
        expected.directories = directory_paths
            .into_iter()
            .map(|(inode, paths)| {
                let has_child = paths.iter().any(|directory| {
                    alive_paths
                        .iter()
                        .any(|candidate| parent_path(candidate) == Some(directory.as_str()))
                });
                let flags = directory_flags::DIR_EXPLICIT
                    | if has_child {
                        0
                    } else {
                        directory_flags::DIR_EMPTY
                    };
                (inode, flags)
            })
            .collect();
        expected.directories.sort_by_key(|(inode, _)| inode.get());

        expected.conflicts = conflicts
            .into_iter()
            .map(|((view_id, inode), mut records)| {
                records.sort_by(|left, right| {
                    (&left.path, left.kind.to_string(), left.line, &left.sides).cmp(&(
                        &right.path,
                        right.kind.to_string(),
                        right.line,
                        &right.sides,
                    ))
                });
                records.dedup();
                (view_id, inode, records)
            })
            .collect();
        Ok(expected)
    }
}

fn collect_expected_conflicts<T>(
    txn: &T,
    store: &ChangeStore,
    view_id: u64,
    visibility: &GraphVisibilityClosure,
    reduced: &super::name_resolution::ReducedPathClaims,
    conflicts: &mut BTreeMap<(u64, Inode), Vec<StoredConflict>>,
) -> Result<(), RepositoryError>
where
    T: GraphTxnT + TreeTxnT + InodeGraphOps<InodeError = PristineError>,
{
    for (path, conflict) in &reduced.conflicts {
        let mut side_hashes = Vec::new();
        for change_id in conflict
            .sides
            .iter()
            .flat_map(|side| side.event_changes.iter().copied())
        {
            let hash = txn
                .get_external(change_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "name-conflict side change {} has no external hash",
                        change_id.get()
                    ))
                })?;
            side_hashes.push(hash.to_base32());
        }
        side_hashes.sort();
        side_hashes.dedup();
        for side in conflict.sides_at_path(path) {
            conflicts
                .entry((view_id, side.inode))
                .or_default()
                .push(StoredConflict {
                    kind: StoredConflictKind::Name,
                    path: path.clone(),
                    line: (!side.is_directory()).then_some(1),
                    sides: side_hashes.clone(),
                });
        }
    }

    for side in &reduced.present {
        if side.is_directory() {
            continue;
        }
        let content = super::content::retrieve_content_with_filter_fast(
            txn,
            store,
            side.inode,
            side.position,
            atomic_core::output::alive::RetrieveOptions::new()
                .with_graph_visibility(visibility.clone()),
        )
        .map_err(|error| RepositoryError::Output(error.to_string()))?;
        if let Some(line) = super::materialize::first_conflict_marker_line(&content) {
            conflicts
                .entry((view_id, side.inode))
                .or_default()
                .push(StoredConflict {
                    kind: StoredConflictKind::Order,
                    path: side.path.clone(),
                    line: Some(line),
                    sides: Vec::new(),
                });
        }
    }
    Ok(())
}

fn visit_reachable_change<T: GraphTxnT>(
    txn: &T,
    change_id: NodeId,
    reachable: &HashSet<NodeId>,
    visiting: &mut HashSet<NodeId>,
    visited: &mut HashSet<NodeId>,
    ordered: &mut Vec<NodeId>,
) -> Result<(), RepositoryError> {
    if visited.contains(&change_id) {
        return Ok(());
    }
    if !visiting.insert(change_id) {
        return Err(RepositoryError::InvalidOperation {
            message: format!("native-index replay found a dependency cycle at {change_id}"),
        });
    }
    for dependency in txn
        .get_indexed_change_deps(change_id)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    {
        let dependency_id = txn
            .get_internal(&dependency)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "change {} has unregistered dependency {}",
                    change_id.get(),
                    dependency
                ))
            })?;
        if reachable.contains(&dependency_id) {
            visit_reachable_change(txn, dependency_id, reachable, visiting, visited, ordered)?;
        }
    }
    visiting.remove(&change_id);
    visited.insert(change_id);
    ordered.push(change_id);
    Ok(())
}

fn insert_tree_pair(
    tree: &mut BTreeMap<String, Inode>,
    reverse: &mut BTreeMap<Inode, String>,
    path: &str,
    inode: Inode,
) -> Result<(), RepositoryError> {
    if let Some(previous) = tree.insert(path.to_string(), inode) {
        if previous != inode {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "native projection maps '{}' to both inode {} and inode {}",
                    path,
                    previous.get(),
                    inode.get()
                ),
            });
        }
    }
    if let Some(previous) = reverse.insert(inode, path.to_string()) {
        if previous != path {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "native projection maps inode {} to both '{}' and '{}'",
                    inode.get(),
                    previous,
                    path
                ),
            });
        }
    }
    Ok(())
}

fn parent_path(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

fn compare_native_derived_indexes<T>(
    txn: &T,
    expected: &NativeDerivedIndexes,
) -> Result<NativeIndexReport, RepositoryError>
where
    T: TreeTxnT + PathClaimTxnT + ViewTxnT,
{
    let mut report = NativeIndexReport {
        expected_rows: native_row_count(expected),
        ..NativeIndexReport::default()
    };

    match txn.path_claim_schema_version() {
        Ok(Some(version)) if version == atomic_core::pristine::PATH_CLAIM_SCHEMA_VERSION => {}
        Ok(version) => report.problems.push(NativeIndexProblem {
            index: NativeIndex::PathClaims,
            kind: if version.is_none() {
                NativeIndexProblemKind::Missing
            } else {
                NativeIndexProblemKind::Mismatched
            },
            key: "<schema>".to_string(),
            expected: Some(atomic_core::pristine::PATH_CLAIM_SCHEMA_VERSION.to_string()),
            actual: version.map(|version| version.to_string()),
        }),
        Err(error) => report.problems.push(NativeIndexProblem {
            index: NativeIndex::PathClaims,
            kind: NativeIndexProblemKind::Malformed,
            key: "<schema>".to_string(),
            expected: Some(atomic_core::pristine::PATH_CLAIM_SCHEMA_VERSION.to_string()),
            actual: Some(error.to_string()),
        }),
    }
    compare_table(
        NativeIndex::PathClaims,
        canonical_path_claims(&expected.path_claims),
        txn.iter_path_claims()
            .map(|rows| canonical_path_claims(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Tree,
        canonical_tree(&expected.tree),
        txn.iter_tree()
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map(|rows| canonical_tree(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::RevTree,
        canonical_rev_tree(&expected.rev_tree),
        txn.iter_rev_tree_pairs()
            .map(|rows| canonical_rev_tree(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Inodes,
        canonical_inodes(&expected.inodes),
        txn.snapshot_inodes().map(|rows| canonical_inodes(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::RevInodes,
        canonical_rev_inodes(&expected.rev_inodes),
        txn.snapshot_rev_inodes()
            .map(|rows| canonical_rev_inodes(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Directories,
        canonical_directories(&expected.directories),
        txn.snapshot_directories()
            .map(|rows| canonical_directories(&rows)),
        &mut report,
    );
    compare_table(
        NativeIndex::Conflicts,
        canonical_conflicts(&expected.conflicts),
        txn.snapshot_conflicts()
            .map(|rows| canonical_conflicts(&rows)),
        &mut report,
    );
    report.problems.sort();
    Ok(report)
}

fn compare_table(
    index: NativeIndex,
    expected: BTreeMap<String, String>,
    actual: Result<BTreeMap<String, String>, PristineError>,
    report: &mut NativeIndexReport,
) {
    let actual = match actual {
        Ok(actual) => actual,
        Err(error) => {
            report.problems.push(NativeIndexProblem {
                index,
                kind: NativeIndexProblemKind::Malformed,
                key: "<table>".to_string(),
                expected: None,
                actual: Some(error.to_string()),
            });
            return;
        }
    };
    report.actual_rows += actual.len();
    for (key, expected_value) in &expected {
        match actual.get(key) {
            None => report.problems.push(NativeIndexProblem {
                index,
                kind: NativeIndexProblemKind::Missing,
                key: key.clone(),
                expected: Some(expected_value.clone()),
                actual: None,
            }),
            Some(actual_value) if actual_value != expected_value => {
                report.problems.push(NativeIndexProblem {
                    index,
                    kind: NativeIndexProblemKind::Mismatched,
                    key: key.clone(),
                    expected: Some(expected_value.clone()),
                    actual: Some(actual_value.clone()),
                });
            }
            Some(_) => {}
        }
    }
    for (key, actual_value) in actual {
        if !expected.contains_key(&key) {
            report.problems.push(NativeIndexProblem {
                index,
                kind: NativeIndexProblemKind::Stale,
                key,
                expected: None,
                actual: Some(actual_value),
            });
        }
    }
}

fn canonical_path_claims(rows: &[PathClaimEntry]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|entry| {
            let event = entry.event;
            (
                format!(
                    "{:?}",
                    (
                        &entry.path,
                        event.event_change,
                        event.operation_index,
                        event.kind,
                        event.state,
                        event.claim,
                    )
                ),
                "present".to_string(),
            )
        })
        .collect()
}

fn canonical_tree(rows: &[(String, Inode)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(path, inode)| (path.clone(), inode.get().to_string()))
        .collect()
}

fn canonical_rev_tree(rows: &[(Inode, String)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(inode, path)| (inode.get().to_string(), path.clone()))
        .collect()
}

fn canonical_inodes(rows: &[(Inode, Position<NodeId>)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(inode, position)| (inode.get().to_string(), position.to_string()))
        .collect()
}

fn canonical_rev_inodes(rows: &[(Position<NodeId>, Inode)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(position, inode)| (position.to_string(), inode.get().to_string()))
        .collect()
}

fn canonical_directories(rows: &[(Inode, u8)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(inode, flags)| (inode.get().to_string(), format!("{flags:#04x}")))
        .collect()
}

fn canonical_conflicts(rows: &[(u64, Inode, Vec<StoredConflict>)]) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(view_id, inode, records)| {
            let mut records = records.clone();
            records.sort_by(|left, right| {
                (&left.path, left.kind.to_string(), left.line, &left.sides).cmp(&(
                    &right.path,
                    right.kind.to_string(),
                    right.line,
                    &right.sides,
                ))
            });
            (format!("{view_id}:{}", inode.get()), format!("{records:?}"))
        })
        .collect()
}

fn native_row_count(indexes: &NativeDerivedIndexes) -> usize {
    indexes.path_claims.len()
        + indexes.tree.len()
        + indexes.rev_tree.len()
        + indexes.inodes.len()
        + indexes.rev_inodes.len()
        + indexes.directories.len()
        + indexes.conflicts.len()
}

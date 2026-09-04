use super::*;
use atomic_core::pristine::{PathClaimTxnT, StoredConflict, StoredConflictKind};

/// Return the 1-based line number of the first Atomic conflict-start marker
/// (`>>>>>>>`) in `content`, or `None` if the content has no markers.
///
/// This is the authoritative signal for "this materialized file is
/// conflicted": it reflects exactly what was written to disk, so persisted
/// conflict state stays in lock-step with the bytes the user sees.
pub(crate) fn first_conflict_marker_line(content: &[u8]) -> Option<u32> {
    let text = match std::str::from_utf8(content) {
        Ok(t) => t,
        Err(_) => return None, // binary content carries no textual markers
    };
    for (idx, line) in text.lines().enumerate() {
        if line.starts_with(">>>>>>>") {
            return Some((idx + 1) as u32);
        }
    }
    None
}

/// True if a `try_lock*` error means the lock is currently held by someone else
/// (as opposed to a genuine I/O failure).
///
/// On Unix a contended non-blocking lock surfaces as `WouldBlock`, but on
/// Windows the OS returns `ERROR_LOCK_VIOLATION` (code 33), which maps to
/// `ErrorKind::Uncategorized` rather than `WouldBlock`. Comparing the raw OS
/// error against `fs2::lock_contended_error()` handles both platforms.
pub(crate) fn is_lock_contended(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
        || (e.raw_os_error().is_some()
            && e.raw_os_error() == fs2::lock_contended_error().raw_os_error())
}

impl Repository {
    /// Return the first working-copy file that still contains an unresolved
    /// conflict marker, as `(path, 1-based line)`, or `None` if the working
    /// copy is clean.
    ///
    /// This scans the same status entries `record` guards on
    /// (Added / Modified / Conflicted) with the same detector
    /// (`first_conflict_marker_line`), so `atomic record` and
    /// `atomic git push` agree on what counts as a conflicted working copy
    /// (SPEC §5.4). It is the shared guard that stops any automated commit path
    /// — including shadow materialization — from baking markers into history.
    /// Try to acquire the repo-scoped shadow-commit lock **without blocking**.
    ///
    /// Returns `Some(guard)` if acquired (the lock is held until the returned
    /// file is dropped), or `None` if another shadow materialize/commit is
    /// already in flight. This serializes the single shadow-commit pipeline
    /// (SPEC §4.3 / Principle 5) so concurrent hooks/commands never interleave
    /// partial staging. It is a distinct lock from the deferred-tree alignment
    /// lock and is meant to be taken **outermost**, before any DB write txn.
    pub fn try_lock_shadow_commit(&self) -> Result<Option<std::fs::File>, RepositoryError> {
        use fs2::FileExt;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dot_dir.join("shadow-commit.lock"))?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(Some(lock)),
            Err(e) if is_lock_contended(&e) => Ok(None),
            Err(e) => Err(RepositoryError::Io(e)),
        }
    }

    pub fn first_working_copy_conflict_marker(
        &self,
    ) -> Result<Option<(String, u32)>, RepositoryError> {
        let status = self.status(StatusOptions::default())?;
        for entry in status.entries() {
            let is_directory = entry.details().map(|d| d == "directory").unwrap_or(false);
            if is_directory {
                continue;
            }
            if !matches!(
                entry.status(),
                FileStatus::Added | FileStatus::Modified | FileStatus::Conflicted
            ) {
                continue;
            }
            let path = entry.path().to_string_lossy().to_string();
            let full_path = self.root.join(&path);
            if let Ok(content) = std::fs::read(&full_path) {
                if let Some(line) = first_conflict_marker_line(&content) {
                    return Ok(Some((path, line)));
                }
            }
        }
        Ok(None)
    }
}

/// Render a name conflict: two or more inodes are alive at the same path on
/// this view, so instead of silently emitting whichever inode `TREE` happened
/// to keep, wrap every side's materialized content in conflict markers.
///
/// The block opens with a `>>>>>>>` line (so the existing marker-driven
/// surfacing pipeline — `first_conflict_marker_line` → `conflicts_by_path` →
/// `persist_view_conflicts` — flags the file exactly as it does for content
/// conflicts), separates sides with `=======`, and closes with `<<<<<<<`,
/// matching Atomic's inverted marker convention. `sides` must already be in a
/// deterministic order so the rendering is stable across runs.
#[allow(clippy::too_many_arguments)]
fn render_name_conflict<C: atomic_core::change::ChangeStore>(
    txn: &atomic_core::pristine::ReadTxn,
    store: &C,
    inode_graph_table: &redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>,
    visibility: &GraphVisibilityClosure,
    external_hashes: &std::collections::HashMap<NodeId, Hash>,
    path: &str,
    sides: &[(Inode, Position<NodeId>)],
) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    for (i, (inode, position)) in sides.iter().enumerate() {
        if i == 0 {
            out.extend_from_slice(format!(">>>>>>> {} (name conflict)\n", path).as_bytes());
        } else {
            out.extend_from_slice(b"=======\n");
        }

        let side = render_name_conflict_side(
            txn,
            store,
            inode_graph_table,
            visibility,
            external_hashes,
            path,
            *inode,
            *position,
        )?;
        out.extend_from_slice(&side);
        if !side.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out.extend_from_slice(format!("<<<<<<< {} (name conflict)\n", path).as_bytes());
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_name_conflict_side<C: atomic_core::change::ChangeStore>(
    txn: &atomic_core::pristine::ReadTxn,
    store: &C,
    inode_graph_table: &redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>,
    visibility: &GraphVisibilityClosure,
    external_hashes: &std::collections::HashMap<NodeId, Hash>,
    path: &str,
    inode: Inode,
    position: Position<NodeId>,
) -> Result<Vec<u8>, String> {
    use atomic_core::output::repo::{
        output_graph_content_resolved, resolve_conflicts_semantically,
    };
    use atomic_core::output::{compute_order, retrieve_graph, RetrieveOptions, Writer};
    use atomic_core::pristine::InodePreloadTxn;

    let preloaded = InodePreloadTxn::from_table(txn, inode, inode_graph_table)
        .map_err(|e| format!("{}: name-conflict preload: {:?}", path, e))?;
    let retrieve_opts = RetrieveOptions::default().with_graph_visibility(visibility.clone());
    let retrieve_result = retrieve_graph(&preloaded, position, retrieve_opts)
        .map_err(|e| format!("{}: name-conflict retrieve: {:?}", path, e))?;
    let mut graph = retrieve_result.graph;
    let order = compute_order(&mut graph);
    let resolved = resolve_conflicts_semantically(&preloaded, store, &graph, &order)
        .map_err(|error| format!("{}: name-conflict semantic resolution: {}", path, error))?;
    let buffer = Vec::with_capacity(graph.total_bytes());
    let mut writer = Writer::new(buffer);
    let hash_fn = |node_id: NodeId| -> Result<Option<Hash>, atomic_core::pristine::PristineError> {
        if node_id.is_root() {
            return Ok(None);
        }
        external_hashes
            .get(&node_id)
            .copied()
            .map(Some)
            .ok_or(atomic_core::pristine::PristineError::ChangeNotFound { id: node_id.get() })
    };
    output_graph_content_resolved(store, hash_fn, &graph, &order, &mut writer, &resolved)
        .map_err(|e| format!("{}: name-conflict content: {:?}", path, e))?;
    Ok(writer.into_inner())
}

struct PreparedNameConflictOutput {
    files: Vec<MaterializedEntry>,
    directories: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_name_conflict_output<C: atomic_core::change::ChangeStore>(
    txn: &atomic_core::pristine::ReadTxn,
    store: &C,
    inode_graph_table: &redb::ReadOnlyMultimapTable<&'static [u8; 32], &'static [u8; 24]>,
    visibility: &GraphVisibilityClosure,
    external_hashes: &std::collections::HashMap<NodeId, Hash>,
    conflicts: &std::collections::HashMap<String, super::name_resolution::ProjectedNameConflict>,
    only_paths: Option<&std::collections::HashSet<String>>,
) -> Result<PreparedNameConflictOutput, RepositoryError> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut paths: Vec<_> = conflicts.keys().cloned().collect();
    paths.sort();
    for path in paths {
        if only_paths.is_some_and(|selected| !selected.contains(&path)) {
            continue;
        }
        let conflict = &conflicts[&path];
        let sides = conflict.sides_at_path(&path);
        if sides.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("name conflict '{}' has no path-local side", path),
            });
        }
        let directories_at_path = sides.iter().filter(|side| side.is_directory()).count();
        if directories_at_path == sides.len() {
            directories.push(path);
            continue;
        }
        if directories_at_path != 0 {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "cannot materialize mixed file/directory name conflict '{}' without choosing a side",
                    path
                ),
            });
        }

        let content = if sides.len() == 1 {
            let side = sides[0];
            render_name_conflict_side(
                txn,
                store,
                inode_graph_table,
                visibility,
                external_hashes,
                &path,
                side.inode,
                side.position,
            )
            .map_err(RepositoryError::Output)?
        } else {
            let render_sides: Vec<_> = sides
                .iter()
                .map(|side| (side.inode, side.position))
                .collect();
            render_name_conflict(
                txn,
                store,
                inode_graph_table,
                visibility,
                external_hashes,
                &path,
                &render_sides,
            )
            .map_err(RepositoryError::Output)?
        };
        files.push(MaterializedEntry::present(path, sides[0].inode, content));
    }
    Ok(PreparedNameConflictOutput { files, directories })
}

impl Repository {
    fn remove_absent_entries(
        &self,
        entries: &[MaterializedEntry],
        only_paths: Option<&std::collections::HashSet<String>>,
        prefix: Option<&str>,
    ) -> Result<usize, RepositoryError> {
        let selected = |path: &str| {
            !only_paths.is_some_and(|paths| !paths.contains(path))
                && !prefix.is_some_and(|prefix| !path.starts_with(prefix))
        };
        let mut removed_files = 0;

        for entry in entries
            .iter()
            .filter(|entry| !entry.is_directory() && selected(entry.path()))
        {
            let absolute = self.root.join(entry.path());
            match std::fs::symlink_metadata(&absolute) {
                Ok(metadata)
                    if metadata.file_type().is_file() || metadata.file_type().is_symlink() =>
                {
                    std::fs::remove_file(&absolute).map_err(|error| {
                        RepositoryError::Output(format!(
                            "failed to remove absent materialized path '{}': {}",
                            entry.path(),
                            error
                        ))
                    })?;
                    removed_files += 1;
                }
                Ok(_) => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "cannot materialize absent file '{}': working-copy path is not a file",
                            entry.path()
                        ),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(RepositoryError::Io(error)),
            }
            self.del_file_index(entry.path())?;
        }

        let mut directories: Vec<_> = entries
            .iter()
            .filter(|entry| entry.is_directory() && selected(entry.path()))
            .collect();
        directories.sort_by_key(|entry| std::cmp::Reverse(entry.path().matches('/').count()));
        for entry in directories {
            let absolute = self.root.join(entry.path());
            match std::fs::symlink_metadata(&absolute) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    std::fs::remove_dir(&absolute).map_err(|error| {
                        RepositoryError::InvalidOperation {
                            message: format!(
                                "cannot remove absent directory '{}': {} (untracked children are never removed recursively)",
                                entry.path(), error
                            ),
                        }
                    })?;
                }
                Ok(_) => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "cannot materialize absent directory '{}': working-copy path is not a directory",
                            entry.path()
                        ),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(RepositoryError::Io(error)),
            }
            self.del_file_index(entry.path())?;
        }

        Ok(removed_files)
    }

    /// Persist the conflict state discovered by a materialize into the
    /// `CONFLICTS` table for `view_id`.
    ///
    /// A full materialize (`only_paths` is `None`) replaces the view's entire
    /// conflict set: every prior entry is dropped and the current conflicts
    /// re-written. A partial materialize updates only the touched files,
    /// setting or clearing each. This keeps a partial run from wrongly
    /// discarding conflicts for files it did not re-materialize.
    fn persist_view_conflicts(
        &self,
        view_id: u64,
        path_to_inode: &std::collections::HashMap<String, u64>,
        conflicts_by_path: &std::collections::HashMap<String, u32>,
        name_conflicts: &std::collections::HashMap<
            String,
            super::name_resolution::ProjectedNameConflict,
        >,
        only_paths: &Option<std::collections::HashSet<String>>,
    ) -> Result<(), RepositoryError> {
        let mut wtxn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut records_by_inode = std::collections::HashMap::<u64, Vec<StoredConflict>>::new();

        if let Some(paths) = only_paths {
            for (inode, records) in wtxn
                .iter_conflicts(view_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let retained: Vec<_> = records
                    .into_iter()
                    .filter(|record| !paths.contains(&record.path))
                    .collect();
                if !retained.is_empty() {
                    records_by_inode.insert(inode, retained);
                }
            }
        }

        for (path, line) in conflicts_by_path {
            if name_conflicts.contains_key(path) {
                continue;
            }
            if let Some(&inode) = path_to_inode.get(path) {
                records_by_inode
                    .entry(inode)
                    .or_default()
                    .push(StoredConflict {
                        kind: StoredConflictKind::Order,
                        path: path.clone(),
                        line: Some(*line),
                        sides: Vec::new(),
                    });
            }
        }

        for (path, conflict) in name_conflicts {
            if only_paths
                .as_ref()
                .is_some_and(|paths| !paths.contains(path))
            {
                continue;
            }
            let mut side_hashes = Vec::new();
            for change_id in conflict
                .sides
                .iter()
                .flat_map(|side| side.event_changes.iter().copied())
            {
                let hash = wtxn
                    .get_external(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
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
            let line = conflicts_by_path.get(path).copied();
            for side in conflict.sides_at_path(path) {
                records_by_inode
                    .entry(side.inode.get())
                    .or_default()
                    .push(StoredConflict {
                        kind: StoredConflictKind::Name,
                        path: path.clone(),
                        line,
                        sides: side_hashes.clone(),
                    });
            }
        }

        wtxn.del_conflicts_prefix(view_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for (inode, mut records) in records_by_inode {
            records.sort_by(|left, right| {
                (&left.path, left.kind.to_string()).cmp(&(&right.path, right.kind.to_string()))
            });
            records.dedup();
            wtxn.put_conflicts(view_id, inode, &records)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        wtxn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }
    /// Compute the set of file paths visible on a view.
    ///
    /// Visibility includes the view's own changes AND all changes
    /// inherited through the parent chain.  A draft view parented on
    /// dev sees dev's files without requiring an explicit insert.
    ///
    /// A file is visible on a view when:
    /// 1. It appears in the global TREE table (has been `add`ed).
    /// 2. Its inode has a graph position in the INODES table (has been
    ///    `record`ed).
    /// 3. The change that introduced that position is visible to the
    ///    view (own changes + parent chain).
    ///
    /// Files that have been `add`ed but not yet `record`ed (no INODES
    /// entry) are NOT returned — they persist across switches as
    /// working-copy state.
    pub fn visible_file_paths(&self, view_name: &str) -> Result<HashSet<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let full_visibility = graph_visibility_closure(&txn, &view)?;
        let visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &full_visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;

        Ok(projection
            .present
            .into_iter()
            .filter_map(|(path, item)| (!item.is_directory).then_some(path))
            .collect())
    }

    /// Materialize the working copy to match the current view's state.
    ///
    /// This synchronizes the working copy files with the repository graph
    /// state for the current view. Files are created, updated, or deleted
    /// to match what's recorded in the view.
    ///
    /// Since all edges are stored in the global GRAPH table, this uses the
    /// raw transaction directly with a change filter to scope which vertices
    /// are alive for this view.
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation including:
    /// - Number of files written
    /// - Number of directories created
    /// - Any conflicts detected
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database cannot be read
    /// - Files cannot be written to the working copy
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    ///
    /// // Reset working copy to current view's state
    /// let result = repo.materialize()?;
    /// println!("Materialized {} files", result.files_written);
    ///
    /// if result.has_conflicts() {
    ///     println!("Warning: {} conflicts detected", result.conflict_count());
    /// }
    /// ```
    pub fn materialize(&self) -> Result<MaterializeResult, RepositoryError> {
        // Use the parallel path — buffers content in memory, processes files
        // concurrently via rayon, writes each file in a single fs::write call,
        // and computes content hashes in-memory (no read-back pass).
        self.materialize_parallel(None)
    }

    /// Sequential materialize fallback.
    ///
    /// Processes files one at a time through the streaming writer path.
    /// Used when the parallel path is not suitable (e.g., memory-constrained
    /// environments).
    pub fn materialize_sequential(&self) -> Result<MaterializeResult, RepositoryError> {
        // PATH_CLAIMS conflict rendering is batch-validated before filesystem
        // effects. Reuse that single implementation so sequential and parallel
        // entry points cannot disagree about a claimant.
        self.materialize_parallel(None)
    }

    /// Materialize only specific files to the working copy.
    ///
    /// This is used after `insert` operations to only rewrite files that
    /// were actually affected by the inserted changes, avoiding a full
    /// rematerialization of the entire working copy.
    ///
    /// Returns the set of `(path, content_hash)` pairs for files that were
    /// written, enabling the caller to update FILE_INDEX without re-reading
    /// from disk.
    pub fn materialize_paths(
        &self,
        paths: std::collections::HashSet<String>,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.materialize_parallel(Some(paths))
    }

    /// Sequentially materialize a specific set of paths.
    pub fn materialize_paths_sequential(
        &self,
        paths: std::collections::HashSet<String>,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.materialize_parallel(Some(paths))
    }

    /// Materialize the working copy using parallel file processing.
    ///
    /// This is an optimized version of `materialize` that:
    /// 1. Buffers each file's content in memory (single allocation per file)
    /// 2. Processes files in parallel using rayon
    /// 3. Writes each file to disk in a single `fs::write` call
    /// 4. Computes content hashes in-memory (no read-back pass)
    ///
    /// File retrieval and write failures are returned to the caller; they are
    /// never converted into successful skips.
    pub fn materialize_parallel(
        &self,
        only_paths: Option<std::collections::HashSet<String>>,
    ) -> Result<MaterializeResult, RepositoryError> {
        let (visibility, view_id) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let view = txn
                .get_view(&self.current_view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: self.current_view.clone(),
                })?;
            (graph_visibility_closure(&txn, &view)?, view.id)
        };
        self.materialize_parallel_with_visibility(only_paths, visibility, view_id)
    }

    pub(super) fn materialize_parallel_with_visibility(
        &self,
        only_paths: Option<std::collections::HashSet<String>>,
        visibility: GraphVisibilityClosure,
        view_id: u64,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.materialize_parallel_with_visibility_mode(only_paths, visibility, view_id, true)
    }

    /// Validate every target render before a switch performs external effects.
    pub(super) fn validate_materialization_with_visibility(
        &self,
        only_paths: Option<std::collections::HashSet<String>>,
        visibility: GraphVisibilityClosure,
        view_id: u64,
    ) -> Result<(), RepositoryError> {
        self.materialize_parallel_with_visibility_mode(only_paths, visibility, view_id, false)
            .map(|_| ())
    }

    fn materialize_parallel_with_visibility_mode(
        &self,
        only_paths: Option<std::collections::HashSet<String>>,
        visibility: GraphVisibilityClosure,
        view_id: u64,
        execute: bool,
    ) -> Result<MaterializeResult, RepositoryError> {
        use atomic_core::output::repo::OutputItem;
        use atomic_core::output::RetrieveOptions;
        use rayon::prelude::*;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view_by_id(view_id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Database(format!(
                    "materialization references missing view id {view_id}"
                ))
            })?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &claim_visibility)?;
        let mut absent_entries = projection.absent;
        let name_conflicts = projection.name_conflicts;
        let mut items: Vec<OutputItem> = projection.present.into_values().collect();
        items.sort_by(|left, right| left.path.cmp(&right.path));

        // Phase 2+4: materialize only lifecycle-present files selected by the caller.
        let file_items: Vec<&OutputItem> = items
            .iter()
            .filter(|item| {
                !item.is_directory
                    && only_paths
                        .as_ref()
                        .is_none_or(|paths| paths.contains(&item.path))
            })
            .collect();

        let total_files = file_items.len();
        let skipped_in_filter = items.iter().filter(|i| !i.is_directory).count() - total_files;

        // Name conflicts come from the causal PATH_CLAIMS projection. TREE and
        // REV_TREE are strict one-to-one caches and are never consulted here.

        let mut result = MaterializeResult::new();
        result.files_skipped += skipped_in_filter;

        // Phase 5a: Pre-warm the ChangeStore cache.
        //
        // Load all changes referenced by file vertices into the cache
        // BEFORE the parallel phase. This ensures:
        // - No disk I/O during parallel execution (all cache hits)
        // - No write-lock contention (peek() uses read locks for hits)
        // - Consistent, predictable per-file performance
        let root = &self.root;
        let store = &self.change_store;

        let trace_mat = std::env::var_os("ATOMIC_TRACE_MATERIALIZE").is_some();
        let mat_start = std::time::Instant::now();

        let external_hashes = {
            let mut change_paths: std::collections::HashMap<NodeId, Vec<String>> =
                std::collections::HashMap::new();
            for item in &file_items {
                if !item.position.change.is_root() {
                    change_paths
                        .entry(item.position.change)
                        .or_default()
                        .push(item.path.clone());
                }
            }
            // Any visible change may own a content vertex reached while rendering.
            for id in visibility.iter_dependency_first().copied() {
                if !id.is_root() {
                    change_paths.entry(id).or_default();
                }
            }

            let mut change_ids_to_warm: Vec<NodeId> = change_paths.keys().copied().collect();
            change_ids_to_warm.sort_by_key(|id| id.get());
            let mut hashes = std::collections::HashMap::with_capacity(change_ids_to_warm.len());
            for node_id in &change_ids_to_warm {
                let mut paths = change_paths.remove(node_id).unwrap_or_default();
                paths.sort();
                paths.dedup();
                let context = if paths.is_empty() {
                    format!("visible change {}", node_id.get())
                } else {
                    format!("change {} for path(s) {}", node_id.get(), paths.join(", "))
                };
                let hash = txn
                    .get_external(*node_id)
                    .map_err(|error| {
                        RepositoryError::Database(format!(
                            "failed to resolve {} while pre-warming materialization: {}",
                            context, error
                        ))
                    })?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!(
                            "failed to resolve {} while pre-warming materialization: missing external hash",
                            context
                        ))
                    })?;
                store.load_change(&hash).map_err(|error| {
                    RepositoryError::Output(format!(
                        "failed to load {} ({}) while pre-warming materialization: {}",
                        context, hash, error
                    ))
                })?;
                hashes.insert(*node_id, hash);
            }
            if trace_mat {
                eprintln!(
                    "[materialize] cache pre-warm complete changes={} elapsed={:?}",
                    change_ids_to_warm.len(),
                    mat_start.elapsed(),
                );
            }
            hashes
        };

        // Phase 5b: Load FILE_INDEX for content-hash skip.
        //
        // If a file already exists on disk with the same content the
        // graph would produce, skip the entire write. This is the
        // Pijul-style "needs_output" check: stat the file, compare
        // hash, and skip if unchanged.
        let file_index: std::collections::HashMap<String, (i64, u32, u64, Hash)> = {
            let idx_txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let entries = idx_txn
                .iter_file_index()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            entries
                .into_iter()
                .map(|(p, s, n, sz, h)| (p, (s, n, sz, h)))
                .collect()
        };

        // A prior view can materialize contested names that intentionally have
        // no TREE row. Remove only paths that are both globally graph-claimed
        // and FILE_INDEX-backed, but absent from the target claim projection.
        // This proves Atomic previously materialized the path and avoids
        // recursively deleting untracked directory contents.
        let target_paths: HashSet<&str> = items
            .iter()
            .map(|item| item.path.as_str())
            .chain(name_conflicts.keys().map(String::as_str))
            .collect();
        let mut globally_claimed_files = HashSet::new();
        let mut globally_claimed_directories = HashSet::new();
        for entry in txn
            .iter_path_claims()
            .map_err(|error| RepositoryError::Database(error.to_string()))?
        {
            match entry.event.kind {
                atomic_core::pristine::PathClaimKind::File => {
                    globally_claimed_files.insert(entry.path);
                }
                atomic_core::pristine::PathClaimKind::Directory => {
                    globally_claimed_directories.insert(entry.path);
                }
            }
        }
        let mut absent_paths: HashSet<String> = absent_entries
            .iter()
            .map(|entry| entry.path().to_string())
            .collect();
        for path in file_index.keys() {
            if globally_claimed_files.contains(path)
                && !globally_claimed_directories.contains(path)
                && !target_paths.contains(path.as_str())
                && only_paths
                    .as_ref()
                    .is_none_or(|selected| selected.contains(path))
                && absent_paths.insert(path.clone())
            {
                absent_entries.push(MaterializedEntry::absent(path, None));
            }
        }

        // Open the INODE_GRAPH table once, shared across all rayon threads.
        // This eliminates per-file open_multimap_table mutex contention.
        let inode_graph_table = txn
            .open_inode_graph_table()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Phase 5c: Process files in parallel — retrieve, order, render, hash,
        // and decide whether a write is needed. Workers never mutate the working copy.
        type FileResult = Result<Option<(MaterializedEntry, Hash, bool, Option<u32>)>, String>;
        let file_results: Vec<FileResult> = file_items
            .par_iter()
            .map(|item| {
                let file_start = std::time::Instant::now();

                // Cloning validated visibility is O(1).
                let retrieve_opts =
                    RetrieveOptions::default().with_graph_visibility(visibility.clone());

                // Inline the output pipeline so we can trace each phase.
                use atomic_core::output::repo::{
                    output_graph_content_resolved, resolve_conflicts_semantically,
                };
                use atomic_core::output::{compute_order, retrieve_graph, Writer};
                use atomic_core::pristine::InodePreloadTxn;

                // Pre-load ALL edges for this file's inode from INODE_GRAPH
                // in a single range scan, then run retrieve_graph over the
                // in-memory HashMap. O(M) scan + O(1) lookups vs O(V×log N)
                // individual B-tree probes.
                let preloaded = InodePreloadTxn::from_table(&txn, item.inode, &inode_graph_table)
                    .map_err(|e| format!("{}: preload: {:?}", item.path, e))?;

                let t_retrieve = std::time::Instant::now();
                let retrieve_result = retrieve_graph(&preloaded, item.position, retrieve_opts)
                    .map_err(|e| format!("{}: retrieve: {:?}", item.path, e))?;

                let vertices = retrieve_result.graph.len_vertices();
                let edges = retrieve_result.edges_traversed;
                let retrieve_ms = t_retrieve.elapsed();
                let mut graph = retrieve_result.graph;

                let (content, order_ms, content_ms) = if graph.is_empty() {
                    (
                        Vec::new(),
                        std::time::Duration::ZERO,
                        std::time::Duration::ZERO,
                    )
                } else {
                    let t_order = std::time::Instant::now();
                    let order = compute_order(&mut graph);
                    let order_ms = t_order.elapsed();

                    let t_content = std::time::Instant::now();
                    let resolved = resolve_conflicts_semantically(
                        &preloaded, store, &graph, &order,
                    )
                    .map_err(|error| format!("{}: semantic resolution: {}", item.path, error))?;
                    let buffer = Vec::with_capacity(graph.total_bytes());
                    let mut writer = Writer::new(buffer);
                    let hash_fn = |node_id: NodeId| -> Result<
                        Option<Hash>,
                        atomic_core::pristine::PristineError,
                    > {
                        if node_id.is_root() {
                            return Ok(None);
                        }
                        external_hashes.get(&node_id).copied().map(Some).ok_or(
                            atomic_core::pristine::PristineError::ChangeNotFound {
                                id: node_id.get(),
                            },
                        )
                    };
                    output_graph_content_resolved(
                        store,
                        hash_fn,
                        &graph,
                        &order,
                        &mut writer,
                        &resolved,
                    )
                    .map_err(|e| format!("{}: content: {:?}", item.path, e))?;
                    (writer.into_inner(), order_ms, t_content.elapsed())
                };

                let entry = MaterializedEntry::present(item.path.clone(), item.inode, content);
                let content = entry
                    .bytes()
                    .expect("parallel renderer always produces a present entry");

                // Detect conflict markers in the materialized bytes. This is
                // the source of truth for persisted conflict state.
                let marker_line = first_conflict_marker_line(content);

                // Compute content hash from the in-memory buffer.
                let content_hash = Hash::of(content);
                let rendered_bytes = content.len() as u64;

                // Content-hash skip: if the file on disk already has this
                // exact content, skip the write entirely.
                if let Some(&(idx_secs, idx_nanos, idx_size, ref idx_hash)) =
                    file_index.get(&item.path)
                {
                    if *idx_hash == content_hash {
                        // Verify the on-disk file still matches the index
                        // (hasn't been modified by the user since last materialize)
                        let abs_path = root.join(&item.path);
                        if let Ok(meta) = std::fs::metadata(&abs_path) {
                            if meta.len() == idx_size {
                                if let Ok(mtime) = meta.modified() {
                                    let dur = mtime
                                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                        .unwrap_or_default();
                                    if dur.as_secs() as i64 == idx_secs
                                        && dur.subsec_nanos() == idx_nanos
                                    {
                                        if trace_mat {
                                            eprintln!(
                                                "[materialize] SKIP {} (content unchanged)",
                                                item.path,
                                            );
                                        }
                                        return Ok(Some((
                                            entry,
                                            content_hash,
                                            false, // not written
                                            marker_line,
                                        )));
                                    }
                                }
                            }
                        }
                    }
                }

                if trace_mat {
                    let elapsed = file_start.elapsed();
                    if elapsed > std::time::Duration::from_millis(50) {
                        eprintln!(
                            "[materialize] SLOW {} bytes={} vertices={} edges={} \
                             retrieve={:?} order={:?} content={:?} total={:?}",
                            item.path,
                            rendered_bytes,
                            vertices,
                            edges,
                            retrieve_ms,
                            order_ms,
                            content_ms,
                            elapsed,
                        );
                    }
                }

                Ok(Some((entry, content_hash, true, marker_line)))
            })
            .collect();

        if trace_mat {
            eprintln!(
                "[materialize] parallel phase complete files={} elapsed={:?}",
                total_files,
                mat_start.elapsed(),
            );
        }

        // Validate the entire render batch before the first working-copy or
        // pristine mutation. Rayon has already evaluated every item into this
        // vector, so any graph/preload/content error aborts the batch here.
        let mut rendered_files = file_results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::Output)?;
        let prepared_name_conflicts = prepare_name_conflict_output(
            &txn,
            store,
            &inode_graph_table,
            &visibility,
            &external_hashes,
            &name_conflicts,
            only_paths.as_ref(),
        )?;
        for entry in prepared_name_conflicts.files {
            let bytes = entry
                .bytes()
                .expect("prepared name-conflict file is present");
            rendered_files.push(Some((
                entry.clone(),
                Hash::of(bytes),
                true,
                first_conflict_marker_line(bytes),
            )));
        }
        for path in name_conflicts.keys() {
            if only_paths.as_ref().is_none_or(|paths| paths.contains(path)) {
                result.add_conflict(atomic_core::output::repo::FileConflict::new(
                    path,
                    atomic_core::output::repo::FileConflictType::Name,
                ));
            }
        }

        if !execute {
            drop(inode_graph_table);
            drop(txn);
            return Ok(result);
        }

        // Files whose materialized content carries conflict markers, with the
        // 1-based line of the first marker.
        let mut conflicts_by_path: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for rendered in rendered_files.iter().flatten() {
            if let Some(line) = rendered.3 {
                conflicts_by_path.insert(rendered.0.path().to_string(), line);
            }
        }

        // Build path→inode while the projected items are still borrowed from
        // the read transaction. Conflict persistence itself happens later.
        let mut path_to_inode: std::collections::HashMap<String, u64> = file_items
            .iter()
            .map(|i| (i.path.clone(), i.inode.get()))
            .collect();
        for (path, conflict) in &name_conflicts {
            if let Some(side) = conflict.sides_at_path(path).first() {
                path_to_inode.insert(path.clone(), side.inode.get());
            }
        }
        for entry in &absent_entries {
            if let Some(inode) = entry.inode() {
                path_to_inode.insert(entry.path().to_string(), inode.get());
            }
        }

        // Execution phase. From this point onward an external filesystem error
        // can leave partial effects; graph/render/preload errors cannot reach it.
        for path in &prepared_name_conflicts.directories {
            let abs_dir = root.join(path);
            if !abs_dir.exists() {
                std::fs::create_dir_all(&abs_dir).map_err(|error| {
                    RepositoryError::Output(format!(
                        "failed to create conflicted directory '{}': {}",
                        path, error
                    ))
                })?;
            }
            result.record_directory();
        }
        for item in &items {
            if !item.is_directory {
                continue;
            }
            let selected = only_paths.as_ref().is_none_or(|paths| {
                paths.contains(&item.path)
                    || paths.iter().any(|path| {
                        path.strip_prefix(&item.path)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                    })
            });
            if !selected {
                result.record_skipped();
                continue;
            }
            let abs_dir = root.join(&item.path);
            if !abs_dir.exists() {
                std::fs::create_dir_all(&abs_dir).map_err(|error| {
                    RepositoryError::Output(format!(
                        "failed to create materialized directory '{}': {}",
                        item.path, error
                    ))
                })?;
            }
            result.record_directory();
        }

        let mut index_entries: Vec<(String, i64, u32, u64, Hash)> = Vec::new();
        for rendered in rendered_files {
            let Some((entry, content_hash, needs_write, _marker_line)) = rendered else {
                result.files_skipped += 1;
                continue;
            };
            if !needs_write {
                result.files_skipped += 1;
                continue;
            }

            let path = entry.path().to_string();
            let content = entry
                .bytes()
                .expect("parallel renderer always produces a present entry");
            let abs_path = root.join(&path);
            if let Some(parent) = abs_path.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        RepositoryError::Output(format!(
                            "failed to create parent for '{}': {}",
                            path, error
                        ))
                    })?;
                }
            }
            std::fs::write(&abs_path, content).map_err(|error| {
                RepositoryError::Output(format!("failed to write '{}': {}", path, error))
            })?;

            result.files_written += 1;
            result.bytes_written += content.len() as u64;

            let metadata = std::fs::metadata(&abs_path).map_err(|error| {
                RepositoryError::Output(format!(
                    "failed to stat materialized path '{}': {}",
                    path, error
                ))
            })?;
            let mtime = metadata.modified().map_err(|error| {
                RepositoryError::Output(format!(
                    "failed to read modification time for '{}': {}",
                    path, error
                ))
            })?;
            let duration = mtime
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            index_entries.push((
                path,
                duration.as_secs() as i64,
                duration.subsec_nanos(),
                metadata.len(),
                content_hash,
            ));
        }

        drop(inode_graph_table);
        drop(txn);

        if !index_entries.is_empty() {
            self.update_file_index(&index_entries)?;
        }
        result.files_deleted +=
            self.remove_absent_entries(&absent_entries, only_paths.as_ref(), None)?;
        self.persist_view_conflicts(
            view_id,
            &path_to_inode,
            &conflicts_by_path,
            &name_conflicts,
            &only_paths,
        )?;

        Ok(result)
    }

    /// Update FILE_INDEX for paths that must be present after materialization.
    ///
    /// Callers pass the lifecycle-present subset, so a missing path is an error;
    /// legitimately absent paths never enter this set.
    #[allow(dead_code)]
    fn populate_file_index_for_paths(
        &self,
        paths: &std::collections::HashSet<String>,
    ) -> Result<(), RepositoryError> {
        use std::time::SystemTime;

        let mut ordered_paths: Vec<&String> = paths.iter().collect();
        ordered_paths.sort();
        let mut entries: Vec<(String, i64, u32, u64, Hash)> =
            Vec::with_capacity(ordered_paths.len());

        for path in ordered_paths {
            let abs_path = self.root.join(path);
            let metadata = std::fs::metadata(&abs_path).map_err(|error| {
                RepositoryError::Output(format!(
                    "failed to stat materialized path '{}': {}",
                    path, error
                ))
            })?;
            if !metadata.is_file() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot index materialized path '{}': path is not a file",
                        path
                    ),
                });
            }

            let mtime = metadata.modified().map_err(|error| {
                RepositoryError::Output(format!(
                    "failed to read modification time for '{}': {}",
                    path, error
                ))
            })?;
            let duration = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let content = std::fs::read(&abs_path).map_err(|error| {
                RepositoryError::Output(format!(
                    "failed to read materialized path '{}': {}",
                    path, error
                ))
            })?;

            entries.push((
                path.clone(),
                duration.as_secs() as i64,
                duration.subsec_nanos(),
                metadata.len(),
                Hash::of(&content),
            ));
        }

        self.update_file_index(&entries)
    }

    /// Materialize the working copy for a specific prefix only.
    ///
    /// This is useful for partial updates when you only want to sync
    /// a subset of files.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to materialize (e.g., "src/")
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation.
    pub fn materialize_prefix(&self, prefix: &str) -> Result<MaterializeResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;
        let full_visibility = graph_visibility_closure(&txn, &view)?;
        let visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &full_visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        let mut paths: HashSet<String> = projection
            .present
            .keys()
            .chain(projection.absent_metadata.keys())
            .chain(projection.name_conflicts.keys())
            .filter(|path| path.starts_with(prefix))
            .cloned()
            .collect();
        for path in projection.present.keys() {
            if prefix
                .strip_prefix(path)
                .is_some_and(|suffix| suffix.starts_with('/'))
            {
                paths.insert(path.clone());
            }
        }
        drop(txn);
        self.materialize_parallel(Some(paths))
    }
}

#[cfg(test)]
mod conflict_marker_tests {
    use super::first_conflict_marker_line;

    #[test]
    fn detects_numbered_start_marker_at_line_start() {
        let content = b"const shared = 1;\n>>>>>>> 1\nconst a = 2;\n======= 1 [C2YTBAHQ]\nconst b = 3;\n<<<<<<< 1\n";
        assert_eq!(first_conflict_marker_line(content), Some(2));
    }

    #[test]
    fn ignores_separator_or_content_that_only_appears_mid_line() {
        // A legitimate line that merely contains `=======` (e.g. a Markdown
        // rule or a comment) must not be misdetected — only a `>>>>>>>` at
        // line start counts.
        let content = b"let divider = \"=======\";\nfn eq() { a ======= b }\n";
        assert_eq!(first_conflict_marker_line(content), None);
    }

    #[test]
    fn clean_content_has_no_marker() {
        assert_eq!(first_conflict_marker_line(b"fn main() {}\n"), None);
    }

    #[test]
    fn binary_content_carries_no_marker() {
        assert_eq!(first_conflict_marker_line(&[0xff, 0xfe, 0x00, 0x01]), None);
    }
}

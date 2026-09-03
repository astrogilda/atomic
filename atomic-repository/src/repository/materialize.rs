use super::*;
use atomic_core::pristine::{CachedGraphTxn, StoredConflict, StoredConflictKind};

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
    use atomic_core::output::repo::{
        output_graph_content_resolved, resolve_conflicts_semantically,
    };
    use atomic_core::output::{compute_order, retrieve_graph, RetrieveOptions, Writer};
    use atomic_core::pristine::InodePreloadTxn;

    let mut out: Vec<u8> = Vec::new();
    for (i, (inode, position)) in sides.iter().enumerate() {
        if i == 0 {
            out.extend_from_slice(format!(">>>>>>> {} (name conflict)\n", path).as_bytes());
        } else {
            out.extend_from_slice(b"=======\n");
        }

        let preloaded = InodePreloadTxn::from_table(txn, *inode, inode_graph_table)
            .map_err(|e| format!("{}: name-conflict preload: {:?}", path, e))?;
        let retrieve_opts = RetrieveOptions::default().with_graph_visibility(visibility.clone());
        let retrieve_result = retrieve_graph(&preloaded, *position, retrieve_opts)
            .map_err(|e| format!("{}: name-conflict retrieve: {:?}", path, e))?;
        let mut graph = retrieve_result.graph;
        let order = compute_order(&mut graph);
        let resolved = resolve_conflicts_semantically(&preloaded, store, &graph, &order)
            .map_err(|error| format!("{}: name-conflict semantic resolution: {}", path, error))?;
        let buffer = Vec::with_capacity(graph.total_bytes());
        let mut writer = Writer::new(buffer);
        let hash_fn =
            |node_id: NodeId| -> Result<Option<Hash>, atomic_core::pristine::PristineError> {
                if node_id.is_root() {
                    return Ok(None);
                }
                external_hashes.get(&node_id).copied().map(Some).ok_or(
                    atomic_core::pristine::PristineError::ChangeNotFound { id: node_id.get() },
                )
            };
        output_graph_content_resolved(store, hash_fn, &graph, &order, &mut writer, &resolved)
            .map_err(|e| format!("{}: name-conflict content: {:?}", path, e))?;
        let side = writer.into_inner();
        out.extend_from_slice(&side);
        if !side.ends_with(b"\n") {
            out.push(b'\n');
        }
    }
    out.extend_from_slice(format!("<<<<<<< {} (name conflict)\n", path).as_bytes());
    Ok(out)
}

impl Repository {
    fn remove_absent_entries(
        &self,
        entries: &[MaterializedEntry],
        only_paths: Option<&std::collections::HashSet<String>>,
        prefix: Option<&str>,
    ) -> Result<usize, RepositoryError> {
        let mut removed = 0;
        for entry in entries {
            let path = entry.path();
            if only_paths.is_some_and(|paths| !paths.contains(path)) {
                continue;
            }
            if prefix.is_some_and(|prefix| !path.starts_with(prefix)) {
                continue;
            }

            let absolute = self.root.join(path);
            match std::fs::symlink_metadata(&absolute) {
                Ok(metadata)
                    if metadata.file_type().is_file() || metadata.file_type().is_symlink() =>
                {
                    std::fs::remove_file(&absolute).map_err(|error| {
                        RepositoryError::Output(format!(
                            "failed to remove absent materialized path '{}': {}",
                            path, error
                        ))
                    })?;
                    removed += 1;
                }
                Ok(_) => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "cannot materialize absent file '{}': working-copy path is not a file",
                            path
                        ),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(RepositoryError::Io(error)),
            }
            self.del_file_index(path)?;
        }
        Ok(removed)
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
        only_paths: &Option<std::collections::HashSet<String>>,
    ) -> Result<(), RepositoryError> {
        let mut wtxn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mk = |path: &str, line: u32| StoredConflict {
            kind: StoredConflictKind::Order,
            path: path.to_string(),
            line: Some(line),
            sides: Vec::new(),
        };

        match only_paths {
            None => {
                wtxn.del_conflicts_prefix(view_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                for (path, line) in conflicts_by_path {
                    if let Some(&inode) = path_to_inode.get(path) {
                        let sc = mk(path, *line);
                        wtxn.put_conflicts(view_id, inode, std::slice::from_ref(&sc))
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                }
            }
            Some(paths) => {
                for path in paths {
                    if let Some(&inode) = path_to_inode.get(path) {
                        if let Some(line) = conflicts_by_path.get(path) {
                            let sc = mk(path, *line);
                            wtxn.put_conflicts(view_id, inode, std::slice::from_ref(&sc))
                                .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        } else {
                            wtxn.del_conflicts(view_id, inode)
                                .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        }
                    }
                }
            }
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
        let visibility = graph_visibility_closure(&txn, &view)?;
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

        let visibility = graph_visibility_closure(&txn, &view)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        let present_paths: HashSet<String> = projection
            .present
            .values()
            .filter(|item| !item.is_directory)
            .map(|item| item.path.clone())
            .collect();
        let absent_entries = projection.absent;

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new()
            .with_graph_visibility(visibility)
            .only_paths(present_paths.clone());

        let mut result = materialize_view(&cached_txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;
        drop(cached_txn);
        drop(txn);

        result.files_deleted += self.remove_absent_entries(&absent_entries, None, None)?;
        self.populate_file_index(&result, &present_paths)?;

        Ok(result)
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

        let visibility = graph_visibility_closure(&txn, &view)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        let present_paths: HashSet<String> = projection
            .present
            .values()
            .filter(|item| !item.is_directory && paths.contains(&item.path))
            .map(|item| item.path.clone())
            .collect();
        let absent_entries = projection.absent;

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new()
            .with_graph_visibility(visibility)
            .only_paths(present_paths.clone());

        let mut result = materialize_view(&cached_txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;
        drop(cached_txn);
        drop(txn);

        result.files_deleted += self.remove_absent_entries(&absent_entries, Some(&paths), None)?;
        // Update FILE_INDEX only for selected paths that should be present.
        self.populate_file_index_for_paths(&present_paths)?;

        Ok(result)
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
        use std::collections::HashSet as StdHashSet;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        let absent_entries = projection.absent;
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

        // ── Name-conflict detection ──────────────────────────────────────
        //
        // TREE is a single-valued path→inode index, so `iter_tree` (and hence
        // `file_items`) exposes only ONE inode per path even when two
        // independent creates on different views both claim it — the later
        // recorder silently wins and the first inode is orphaned (rubric A12,
        // ATOM::29/30). Walk REV_TREE to recover every inode that claims each
        // path; when ≥ 2 of them are BOTH visible (creating change in the
        // filter) and alive under this view, the path is a genuine name
        // conflict that must be surfaced rather than silently collapsed.
        //
        // The (relatively expensive) aliveness probe runs ONLY for paths with
        // ≥ 2 candidate inodes, so the common single-inode file pays nothing.
        let name_conflicts: std::collections::HashMap<String, Vec<(Inode, Position<NodeId>)>> = {
            use atomic_core::pristine::TreeTxnT;
            let mut by_path: std::collections::HashMap<String, Vec<Inode>> =
                std::collections::HashMap::new();
            let pairs = txn
                .iter_rev_tree()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for (inode, path) in pairs {
                by_path.entry(path).or_default().push(inode);
            }
            let filter = &visibility;
            let mut conflicts: std::collections::HashMap<String, Vec<(Inode, Position<NodeId>)>> =
                std::collections::HashMap::new();
            for item in &file_items {
                let candidates = match by_path.get(&item.path) {
                    Some(c) if c.len() >= 2 => c,
                    _ => continue,
                };
                let mut live: Vec<(Inode, Position<NodeId>)> = Vec::new();
                for &inode in candidates {
                    let Some(pos) = txn
                        .inode_position(inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                    else {
                        continue;
                    };
                    if !pos.change.is_root() && !filter.contains(&pos.change) {
                        continue; // not visible on this view
                    }
                    if crate::repository::status::is_file_alive_via_retrieval(
                        &txn, inode, pos, filter,
                    )? {
                        live.push((inode, pos));
                    }
                }
                if live.len() >= 2 {
                    // Deterministic order: creating change, then position, then inode.
                    live.sort_by_key(|(ino, p)| (p.change.get(), p.pos.get(), ino.get()));
                    conflicts.insert(item.path.clone(), live);
                }
            }
            conflicts
        };

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

                // Name-conflict override (rare): when ≥ 2 inodes are alive at
                // this path on the view, replace the single-inode content with
                // a marker-wrapped rendering of every side so the conflict is
                // surfaced instead of silently collapsed (rubric A12).
                let content = match name_conflicts.get(&item.path) {
                    Some(sides) => render_name_conflict(
                        &txn,
                        store,
                        &inode_graph_table,
                        &visibility,
                        &external_hashes,
                        &item.path,
                        sides,
                    )?,
                    None => content,
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
        let rendered_files = file_results
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::Output)?;

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
        for entry in &absent_entries {
            if let Some(inode) = entry.inode() {
                path_to_inode.insert(entry.path().to_string(), inode.get());
            }
        }

        // Execution phase. From this point onward an external filesystem error
        // can leave partial effects; graph/render/preload errors cannot reach it.
        let file_paths: StdHashSet<&str> = file_items.iter().map(|i| i.path.as_str()).collect();
        for item in &items {
            if !item.is_directory {
                continue;
            }
            let dir_prefix = format!("{}/", item.path);
            let has_children = file_paths.iter().any(|path| path.starts_with(&dir_prefix));
            if !has_children {
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
        self.persist_view_conflicts(view_id, &path_to_inode, &conflicts_by_path, &only_paths)?;

        Ok(result)
    }

    /// Populate FILE_INDEX for the tracked paths that the lifecycle projection
    /// says must be present after materialization.
    ///
    /// Tracked paths that are absent in this view are intentionally excluded;
    /// every selected present path must stat and read successfully.
    fn populate_file_index(
        &self,
        _result: &MaterializeResult,
        present_paths: &std::collections::HashSet<String>,
    ) -> Result<(), RepositoryError> {
        let tracked = self.list_tracked_files()?;
        let tracked_paths: std::collections::HashSet<String> = tracked
            .iter()
            .map(|file| file.path.to_string_lossy().replace('\\', "/"))
            .collect();
        if let Some(path) = present_paths
            .iter()
            .find(|path| !tracked_paths.contains(path.as_str()))
        {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "cannot index materialized path '{}': path is not tracked",
                    path
                ),
            });
        }
        self.populate_file_index_for_paths(present_paths)
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

        // Get the current view for the change filter
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        let visibility = graph_visibility_closure(&txn, &view)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        let present_paths: HashSet<String> = projection
            .present
            .values()
            .filter(|item| !item.is_directory && item.path.starts_with(prefix))
            .map(|item| item.path.clone())
            .collect();
        let absent_entries = projection.absent;

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new()
            .prefix(prefix)
            .with_graph_visibility(visibility)
            .only_paths(present_paths.clone());

        let mut result = materialize_view(&cached_txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;
        drop(cached_txn);
        drop(txn);

        result.files_deleted += self.remove_absent_entries(&absent_entries, None, Some(prefix))?;
        self.populate_file_index(&result, &present_paths)?;

        Ok(result)
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

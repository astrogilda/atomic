use std::collections::HashMap;

use super::*;

#[derive(Debug, Clone, Copy)]
enum UntrackedScanPolicy {
    Always,
    Never,
    WhenRegularFileDeleted,
}

impl Repository {
    // Status Methods

    /// Compute the status of the working copy.
    ///
    /// Optimized for repositories with tens of thousands of files:
    ///
    /// 1. **Single TREE pass** — builds tracked_paths + inode_map together
    /// 2. **FILE_INDEX fast path** — stat-only check for unchanged files
    /// 3. **Clean files skipped** — only Modified/Added/Deleted/Untracked allocated
    /// 4. **Deferred walkdir** — filesystem walk only when untracked files requested
    ///
    /// # Performance
    ///
    /// | Repo size | Before | After |
    /// |-----------|--------|-------|
    /// | 1,000 files | ~1s | <50ms |
    /// | 43,000 files | ~150s | <3s |
    /// | 80,000 files | ~150s | <5s |
    pub fn status(
        &self,
        working_copy: WorkingCopyId,
        options: StatusOptions,
    ) -> Result<RepositoryStatus, RepositoryError> {
        self.status_inner(
            working_copy,
            options,
            UntrackedScanPolicy::Always,
            true,
            false,
        )
    }

    /// Compute status for recording without scanning unrelated untracked files.
    ///
    /// Raw rename detection is the only record stage that consumes untracked
    /// entries. When it is enabled, defer the walk until the selected tracked
    /// status contains a deleted regular file. Untracked content hashes are
    /// also unnecessary because rename matching reads candidates on demand.
    pub(crate) fn status_for_record(
        &self,
        working_copy: WorkingCopyId,
        options: StatusOptions,
        detect_raw_renames: bool,
        include_all_untracked: bool,
    ) -> Result<RepositoryStatus, RepositoryError> {
        let policy = if include_all_untracked {
            UntrackedScanPolicy::Always
        } else if detect_raw_renames {
            UntrackedScanPolicy::WhenRegularFileDeleted
        } else {
            UntrackedScanPolicy::Never
        };
        self.status_inner(working_copy, options, policy, false, true)
    }

    fn status_inner(
        &self,
        working_copy: WorkingCopyId,
        options: StatusOptions,
        untracked_policy: UntrackedScanPolicy,
        hash_untracked: bool,
        for_record: bool,
    ) -> Result<RepositoryStatus, RepositoryError> {
        use std::time::SystemTime;

        let view_name = self.desired_view_name(working_copy)?;
        let overall_start = std::time::Instant::now();

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &claim_visibility)?;
        let projected_present_paths: HashSet<PathBuf> =
            projection.present.into_keys().map(PathBuf::from).collect();
        let projected_absent: Vec<_> = projection.absent_metadata.into_values().collect();
        let projected_name_conflicts = projection.name_conflicts;
        let persisted_name_paths: HashSet<String> = txn
            .iter_conflicts(view.id)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .into_iter()
            .flat_map(|(_, records)| records)
            .filter(|record| record.kind == atomic_core::pristine::StoredConflictKind::Name)
            .map(|record| record.path)
            .collect();

        let mut status = RepositoryStatus::new(view_name, Some(view.state));

        let phase1_ms = overall_start.elapsed().as_millis();
        log::debug!("status: view filter setup took {}ms", phase1_ms);

        // ── Single-pass TREE scan ──────────────────────────────────────
        let tree_start = std::time::Instant::now();
        //
        // Build tracked_paths, inode_map, and directory_inodes in ONE
        // iter_tree() call instead of three passes.
        let mut tracked_paths: HashSet<PathBuf> = HashSet::new();
        let mut inode_map: HashMap<PathBuf, atomic_core::types::Inode> = HashMap::new();
        let mut directory_inodes: HashSet<atomic_core::types::Inode> = HashSet::new();
        // Cache inode → has_graph_content so we don't call inode_position twice
        let mut has_graph_content_cache: HashMap<PathBuf, bool> = HashMap::new();
        // Files tracked globally (in TREE) but whose introducing change
        // belongs to another view.  These must stay in `tracked_paths` so
        // they don't surface as "Untracked" in the filesystem walk, but if
        // they are absent from disk they must be silently skipped (not
        // reported as "Deleted" — they were never on this view).
        let mut foreign_paths: HashSet<PathBuf> = HashSet::new();

        let tree_iter = txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in tree_iter {
            let (path, inode) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            // Normalize path once (before view filter so foreign_paths
            // uses the same key as tracked_paths).
            let normalized = normalize_tracked_path(&path, &self.root);

            // View filter: decide whether this file's graph content is
            // visible on the current view.
            //
            // Files in TREE without a graph position → Added (not yet
            // recorded).  Files whose creating change IS in the current
            // view's filter → tracked normally.  Files whose creating
            // change is NOT in the filter → "foreign": they belong to
            // another view.  We keep them in tracked_paths (so the
            // filesystem walk doesn't mark them Untracked) but flag them
            // so that if they're absent from disk we skip them silently
            // instead of reporting Deleted.
            let has_graph = match txn
                .inode_position(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                Some(position)
                    if !position.change.is_root() && !visibility.contains(position.change) =>
                {
                    // Foreign file — tracked globally, not on this view.
                    foreign_paths.insert(normalized.clone());
                    false
                }
                Some(_) => true,
                None => false,
            };

            // Apply path filter if specified
            if !options.path_filters.is_empty() {
                let matches = options
                    .path_filters
                    .iter()
                    .any(|f| normalized.starts_with(f) || f.starts_with(&normalized));
                if !matches {
                    continue;
                }
            }

            // Track directory status
            if txn.is_directory(inode).unwrap_or(false) {
                directory_inodes.insert(inode);
            }

            inode_map.insert(normalized.clone(), inode);
            has_graph_content_cache.insert(normalized.clone(), has_graph);
            tracked_paths.insert(normalized);
        }

        // TREE is aligned to the current visible lifecycle and therefore omits
        // recorded deletions. Reintroduce those paths for status classification
        // using the operation-aware projection so a filesystem reappearance is
        // an undelete of the original inode rather than an untracked new file.
        for absent in &projected_absent {
            let normalized = PathBuf::from(&absent.path);
            if !options.path_filters.is_empty() {
                let matches = options
                    .path_filters
                    .iter()
                    .any(|f| normalized.starts_with(f) || f.starts_with(&normalized));
                if !matches {
                    continue;
                }
            }
            tracked_paths.insert(normalized.clone());
            inode_map.insert(normalized.clone(), absent.inode);
            has_graph_content_cache.insert(normalized.clone(), true);
            if absent.directory {
                directory_inodes.insert(absent.inode);
            }
        }

        let tree_ms = tree_start.elapsed().as_millis();
        log::debug!(
            "status: TREE scan took {}ms ({} tracked files, {} dirs)",
            tree_ms,
            tracked_paths.len(),
            directory_inodes.len()
        );

        for path in projected_name_conflicts.keys() {
            let normalized = PathBuf::from(path);
            if options.path_filters.is_empty()
                || options
                    .path_filters
                    .iter()
                    .any(|filter| normalized.starts_with(filter) || filter.starts_with(&normalized))
            {
                tracked_paths.insert(normalized);
            }
        }

        // ── Batch-load FILE_INDEX ───────────────────────────────────────
        //
        // One sequential B-tree scan loads the entire FILE_INDEX into memory.
        // This replaces 43k individual B-tree lookups with 43k HashMap lookups
        // (nanoseconds each).
        let index_start = std::time::Instant::now();
        let file_index_entries = txn
            .iter_working_copy_file_index(working_copy)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let file_index: HashMap<String, (i64, u32, u64, Hash)> = file_index_entries
            .into_iter()
            .map(|(path, secs, nanos, size, hash)| (path, (secs, nanos, size, hash)))
            .collect();
        let index_ms = index_start.elapsed().as_millis();
        log::debug!(
            "status: FILE_INDEX loaded {}ms ({} entries)",
            index_ms,
            file_index.len()
        );

        // ── Classify tracked files ──────────────────────────────────────
        //
        // For each tracked file: check in-memory FILE_INDEX HashMap
        // (mtime+size), stat if needed, hash only when mtime changed.
        // Clean files are skipped.
        let classify_start = std::time::Instant::now();
        let mut found_on_disk: HashSet<PathBuf> = HashSet::new();
        let mut stat_count = 0u64;
        let mut index_hit_count = 0u64;
        let mut hash_count = 0u64;

        for path in &tracked_paths {
            let abs_path = self.root.join(path);
            let inode = inode_map.get(path).copied();
            let has_graph = has_graph_content_cache.get(path).copied().unwrap_or(false);

            let is_dir = inode
                .map(|i| directory_inodes.contains(&i))
                .unwrap_or(false);

            // Skip tracked directories — handle separately
            if is_dir {
                found_on_disk.insert(path.clone());
                let projected_present = projected_present_paths.contains(path);
                if abs_path.is_dir() {
                    if !has_graph || !projected_present {
                        // A projected-absent directory that reappears is an
                        // undelete candidate carrying its original inode.
                        let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Added);
                        if let Some(inode) = inode {
                            entry.set_inode(inode);
                        }
                        entry.set_details("directory".to_string());
                        status.add_entry(entry);
                    }
                } else if has_graph && !projected_present {
                    // The deletion is already recorded on this view.
                } else {
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Deleted);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    entry.set_details("directory".to_string());
                    status.add_entry(entry);
                }
                continue;
            }

            // Check if file exists on disk
            stat_count += 1;
            let metadata = match std::fs::metadata(&abs_path) {
                Ok(m) if m.is_file() => m,
                _ => {
                    // Foreign file not on disk — skip silently.
                    // This file is tracked globally (in TREE) but its
                    // graph content belongs to another view and it does
                    // not exist on disk for THIS view.  Reporting it as
                    // "Deleted" would be wrong (it was never present on
                    // this view).
                    if foreign_paths.contains(path) {
                        found_on_disk.insert(path.clone());
                        continue;
                    }

                    // Lifecycle projection, not content length, decides whether
                    // the missing path is an already-recorded deletion. A
                    // present zero-byte file remains in this set and is reported
                    // Deleted when missing from disk.
                    if has_graph && !projected_present_paths.contains(path) {
                        found_on_disk.insert(path.clone());
                        continue;
                    }
                    // File is genuinely missing and deletion not yet recorded
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Deleted);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    status.add_entry(entry);
                    found_on_disk.insert(path.clone());
                    continue;
                }
            };

            found_on_disk.insert(path.clone());

            if has_graph && !projected_present_paths.contains(path) {
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Added);
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }
                if options.hash_contents {
                    if let Ok(hash) = hash_file_contents(&abs_path) {
                        entry.set_current_hash(hash);
                    }
                }
                status.add_entry(entry);
                continue;
            }

            // Not yet recorded → Added
            if !has_graph {
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Added);
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }
                if options.hash_contents {
                    if let Ok(hash) = hash_file_contents(&abs_path) {
                        entry.set_current_hash(hash);
                    }
                }
                status.add_entry(entry);
                continue;
            }

            // ── FILE_INDEX fast path (ALWAYS runs, not gated on hash_contents) ──
            //
            // Check mtime+size against the in-memory FILE_INDEX HashMap.
            // This is the critical performance path: 99%+ of files in a
            // large repo are clean, and this catches them with just a stat
            // + HashMap lookup (nanoseconds, not B-tree milliseconds).
            let path_str = path.to_string_lossy();
            if let Some(&(cached_secs, cached_nanos, cached_size, cached_hash)) =
                file_index.get(path_str.as_ref())
            {
                let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let duration = mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default();
                let current_secs = duration.as_secs() as i64;
                let current_nanos = duration.subsec_nanos();
                let current_size = metadata.len();

                if current_secs == cached_secs
                    && current_nanos == cached_nanos
                    && current_size == cached_size
                {
                    // mtime + size match → Clean — skip entirely
                    index_hit_count += 1;
                    continue;
                }

                // mtime or size differ — hash to confirm
                if options.hash_contents {
                    hash_count += 1;
                    match hash_file_contents(&abs_path) {
                        Ok(current_hash) => {
                            if current_hash == cached_hash {
                                // Content unchanged (just mtime drift) → Clean
                                continue;
                            }
                            // Content changed → Modified
                            let mut entry =
                                FileStatusEntry::new(path.clone(), FileStatus::Modified);
                            if let Some(inode) = inode {
                                entry.set_inode(inode);
                            }
                            entry.set_current_hash(current_hash);
                            status.add_entry(entry);
                            continue;
                        }
                        Err(_) => {
                            let mut entry =
                                FileStatusEntry::new(path.clone(), FileStatus::Modified);
                            if let Some(inode) = inode {
                                entry.set_inode(inode);
                            }
                            entry.set_details("Unable to read file contents".to_string());
                            status.add_entry(entry);
                            continue;
                        }
                    }
                } else {
                    // No hash requested but mtime changed → assume Modified
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    status.add_entry(entry);
                    continue;
                }
            }

            // No FILE_INDEX entry — file is tracked with graph content
            // but was never indexed. This happens after `atomic insert`,
            // `atomic clone`, or `atomic view switch` materializes a file
            // into the working copy without going through `record()` (only
            // record/materialize_view populate FILE_INDEX today).
            //
            // We CANNOT silently treat this as Clean: that would let real
            // edits to such files become invisible to status/diff/record,
            // and `record(all=true)` would silently drop them.
            //
            // Conservative correctness: mark Modified so the caller can
            // run a full diff against pristine. If the file is actually
            // unchanged, the recording workflow produces an empty hunk
            // and skips it (record_modified_file returns is_empty()).
            // Subsequent records re-populate FILE_INDEX, returning the
            // file to the fast path.
            status.add_stale_index_hit();
            if options.hash_contents {
                hash_count += 1;
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }
                match hash_file_contents(&abs_path) {
                    Ok(current_hash) => {
                        entry.set_current_hash(current_hash);
                    }
                    Err(_) => {
                        entry.set_details("Unable to read file contents".to_string());
                    }
                }
                entry.set_details("FILE_INDEX entry missing".to_string());
                status.add_entry(entry);
            } else {
                // Fast mode: skip the hash but still surface the entry so
                // it isn't silently dropped. Callers using fast mode (e.g.
                // the agent record path) re-query with hash_contents=true
                // before recording.
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }
                entry.set_details("FILE_INDEX entry missing".to_string());
                status.add_entry(entry);
            }
        }

        let classify_ms = classify_start.elapsed().as_millis();
        log::debug!(
            "status: classify took {}ms (stat={}, index_hit={}, hashed={})",
            classify_ms,
            stat_count,
            index_hit_count,
            hash_count
        );

        // ── Deleted files ──────────────────────────────────────────────
        //
        // Any tracked path not found on disk in the loop above is deleted.
        // (Already handled inline above for regular files and directories.)

        // ── Filesystem walk for untracked files ────────────────────────
        //
        // Only do the expensive walkdir when the caller wants untracked
        // files.  The walk skips .atomic, .git, and ignored paths.
        let untracked_start = std::time::Instant::now();
        let scan_untracked = options.include_untracked
            && match untracked_policy {
                UntrackedScanPolicy::Always => true,
                UntrackedScanPolicy::Never => false,
                UntrackedScanPolicy::WhenRegularFileDeleted => status
                    .entries()
                    .iter()
                    .any(|e| e.status() == FileStatus::Deleted && e.details() != Some("directory")),
            };
        log::debug!(
            "status: untracked policy={:?}, scan={}",
            untracked_policy,
            scan_untracked
        );
        if scan_untracked {
            let rules = if options.respect_ignore_files {
                Some(self.load_ignore_rules())
            } else {
                None
            };

            let working_files =
                collect_working_copy_files_with_rules(&self.root, &options, rules.as_ref())
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;

            for path in working_files {
                if !tracked_paths.contains(&path) {
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Untracked);
                    if hash_untracked && options.hash_contents {
                        let abs_path = self.root.join(&path);
                        if let Ok(hash) = hash_file_contents(&abs_path) {
                            entry.set_current_hash(hash);
                        }
                    }
                    status.add_entry(entry);
                }
            }
        }

        // PATH_CLAIMS conflicts are authoritative and visible even before a
        // materialize has persisted compatibility conflict rows. A12 file
        // conflicts become recordable Modified entries once markers are removed;
        // A11 and directory conflicts have no in-file marker channel.
        for (path, conflict) in &projected_name_conflicts {
            let normalized = PathBuf::from(path);
            if !options.path_filters.is_empty()
                && !options
                    .path_filters
                    .iter()
                    .any(|filter| normalized.starts_with(filter) || filter.starts_with(&normalized))
            {
                continue;
            }
            let abs_path = self.root.join(&normalized);
            let has_markers = std::fs::read(&abs_path)
                .ok()
                .and_then(|bytes| super::materialize::first_conflict_marker_line(&bytes))
                .is_some();
            let path_sides = conflict.sides_at_path(path);
            let marker_resolved_a12 = for_record
                && persisted_name_paths.contains(path)
                && !conflict.is_rename_conflict()
                && !has_markers
                && path_sides.iter().all(|side| !side.is_directory())
                && abs_path.is_file();
            let mut entry = FileStatusEntry::new(
                normalized,
                if marker_resolved_a12 {
                    FileStatus::Modified
                } else {
                    FileStatus::Conflicted
                },
            );
            if path_sides.len() == 1 {
                entry.set_inode(path_sides[0].inode);
            }
            entry.set_details(format!(
                "name conflict ({} path(s), {} claimant(s))",
                conflict.paths.len(),
                conflict.sides.len()
            ));
            status.add_or_replace_entry(entry);
        }

        // ── Conflicted files ────────────────────────────────────────────
        //
        // Surface persisted conflict state (written by the last materialize
        // on this view) so a conflicted working tree is never reported clean.
        // A Conflicted entry supersedes any Modified entry for the same path.
        let conflicts = txn
            .iter_conflicts(view.id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for (inode, records) in conflicts {
            let Some(first) = records.first() else {
                continue;
            };
            let path = PathBuf::from(&first.path);
            // Honesty invariant: only report Conflicted while the file on
            // disk still carries markers. Once the user resolves them the
            // file falls back to normal Modified detection and becomes
            // recordable again (which then clears the stale entry).
            let abs_path = self.root.join(&path);
            let still_conflicted = std::fs::read(&abs_path)
                .ok()
                .and_then(|c| super::materialize::first_conflict_marker_line(&c))
                .is_some();
            if !still_conflicted {
                continue;
            }
            let detail = if records.len() > 1 {
                format!("{} ({} conflicts)", first.summary(), records.len())
            } else {
                first.summary()
            };
            let mut entry = FileStatusEntry::new(path, FileStatus::Conflicted);
            entry.set_inode(atomic_core::types::Inode::new(inode));
            entry.set_details(detail);
            // Conflicted supersedes any prior (e.g. Modified) entry so the
            // file is reported exactly once.
            status.add_or_replace_entry(entry);
        }

        let untracked_ms = untracked_start.elapsed().as_millis();
        let total_ms = overall_start.elapsed().as_millis();
        if total_ms > 100 {
            log::warn!(
                "status: total={}ms (view_filter={}ms tree_scan={}ms index_load={}ms classify={}ms untracked={}ms)",
                total_ms,
                phase1_ms,
                tree_ms,
                index_ms,
                classify_ms,
                untracked_ms
            );
        } else {
            log::debug!(
                "status: total={}ms (view_filter={}ms tree_scan={}ms index_load={}ms classify={}ms untracked={}ms)",
                total_ms,
                phase1_ms,
                tree_ms,
                index_ms,
                classify_ms,
                untracked_ms
            );
        }

        Ok(status)
    }

    /// List the current view's persisted conflicts.
    ///
    /// Returns `(path, conflicts)` pairs for every conflicted file, sorted by
    /// path. Honesty invariant: a file is included only while its on-disk
    /// content still carries conflict markers (matching
    /// [`Repository::status`]); a resolved-but-not-yet-recorded file is
    /// omitted. Read-only.
    #[allow(clippy::type_complexity)]
    pub fn list_conflicts(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<(String, Vec<atomic_core::pristine::StoredConflict>)>, RepositoryError> {
        let view_name = self.desired_view_name(working_copy)?;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = match txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        let full_visibility = graph_visibility_closure(&txn, &view)?;
        let visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &full_visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        let active_name_paths: HashSet<String> = projection
            .name_conflicts
            .iter()
            .filter_map(|(path, conflict)| {
                let sides = conflict.sides_at_path(path);
                let requires_marker =
                    !conflict.is_rename_conflict() && sides.iter().all(|side| !side.is_directory());
                let has_marker = std::fs::read(self.root.join(path))
                    .ok()
                    .and_then(|bytes| super::materialize::first_conflict_marker_line(&bytes))
                    .is_some();
                (!requires_marker || has_marker).then(|| path.clone())
            })
            .collect();
        let mut by_path = HashMap::<String, Vec<atomic_core::pristine::StoredConflict>>::new();
        for (_inode, records) in txn
            .iter_conflicts(view.id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            for record in records {
                let still_conflicted = if record.kind
                    == atomic_core::pristine::StoredConflictKind::Name
                {
                    active_name_paths.contains(&record.path)
                } else {
                    std::fs::read(self.root.join(&record.path))
                        .ok()
                        .and_then(|bytes| super::materialize::first_conflict_marker_line(&bytes))
                        .is_some()
                };
                if still_conflicted {
                    by_path.entry(record.path.clone()).or_default().push(record);
                }
            }
        }
        for (path, conflict) in projection.name_conflicts {
            if !active_name_paths.contains(&path) {
                continue;
            }
            by_path.entry(path.clone()).or_insert_with(|| {
                vec![atomic_core::pristine::StoredConflict {
                    kind: atomic_core::pristine::StoredConflictKind::Name,
                    path,
                    line: None,
                    sides: conflict
                        .sides
                        .iter()
                        .flat_map(|side| side.event_changes.iter())
                        .map(|change| change.get().to_string())
                        .collect(),
                }]
            });
        }
        let mut out: Vec<_> = by_path.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Quick status check — uses default options.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_quick(working_copy)?;
    /// println!("Modified: {}", status.modified_count());
    /// ```
    pub fn status_quick(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<RepositoryStatus, RepositoryError> {
        self.status(working_copy, StatusOptions::fast())
    }

    /// Status showing only tracked files (no untracked).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_tracked(working_copy)?;
    /// // Only shows modified, deleted, added - no untracked
    /// ```
    pub fn status_tracked(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<RepositoryStatus, RepositoryError> {
        self.status(working_copy, StatusOptions::tracked_only())
    }

    /// Check if the working copy is clean (no modifications).
    pub fn is_working_copy_clean(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<bool, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::fast())?;
        Ok(status.is_clean())
    }

    /// Get only modified files.
    pub fn modified_files(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::default())?;
        Ok(status.modified().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get only untracked files.
    pub fn untracked_files(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::default())?;
        Ok(status.untracked().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get only deleted files.
    pub fn deleted_files(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(working_copy, StatusOptions::default())?;
        Ok(status.deleted().map(|e| e.path().to_path_buf()).collect())
    }
}

/// Check whether an explicit delete still has surviving content spans.
///
/// This is only a tie-breaker for lifecycle projection: an ordinary tracked
/// empty file is present because its path lifecycle says so. For a visible
/// `FileDel`, surviving spans from concurrent modifications keep the file
/// present; a graph with no live byte spans leaves the deletion absent.
pub(crate) fn is_file_alive_via_retrieval<T: GraphTxnT>(
    txn: &T,
    _inode: Inode,
    position: Position<NodeId>,
    visibility: &GraphVisibilityClosure,
) -> Result<bool, RepositoryError> {
    use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};

    let options = RetrieveOptions::new().with_graph_visibility(visibility.clone());
    let retrieved = retrieve_graph(txn, position, options)
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    Ok(retrieved.graph.total_bytes() > 0)
}

/// Normalize a tracked path from the TREE table to a relative PathBuf
/// with forward slashes, handling absolute paths and platform differences.
fn normalize_tracked_path(path: &str, repo_root: &Path) -> PathBuf {
    let path_buf = PathBuf::from(path);

    let stripped = if path_buf.is_absolute() {
        if let Ok(rel) = path_buf.strip_prefix(repo_root) {
            rel.to_path_buf()
        } else if let Ok(canonical_root) = repo_root.canonicalize() {
            if let Ok(rel) = path_buf.strip_prefix(&canonical_root) {
                rel.to_path_buf()
            } else {
                path_buf
            }
        } else {
            path_buf
        }
    } else {
        path_buf
    };

    // Normalize to forward slashes for cross-platform consistency
    if cfg!(windows) || stripped.to_string_lossy().contains('\\') {
        PathBuf::from(stripped.to_string_lossy().replace('\\', "/"))
    } else {
        stripped
    }
}

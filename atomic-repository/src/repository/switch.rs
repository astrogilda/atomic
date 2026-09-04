use super::*;

/// Return the workspace directory path for a given view.
///
/// The path is `.atomic/workspaces/<view_name>/`.  View names may
/// contain `/` (e.g. `agent/ses_abc123`), which becomes a nested
/// directory structure.
pub(super) fn workspace_path(dot_dir: &Path, view_name: &str) -> PathBuf {
    dot_dir.join(WORKSPACES_DIR).join(view_name)
}

/// Ensure the workspace directory for a view exists.
///
/// Creates `.atomic/workspaces/<view_name>/` and any intermediate
/// directories.  This is called from `init`, `create_view`, and
/// `create_view_from`.
pub(super) fn ensure_workspace_dir(dot_dir: &Path, view_name: &str) -> Result<(), RepositoryError> {
    let ws = workspace_path(dot_dir, view_name);
    std::fs::create_dir_all(&ws)?;
    Ok(())
}

fn working_copy_workspace_path(
    dot_dir: &Path,
    working_copy: WorkingCopyId,
    view_name: &str,
) -> PathBuf {
    dot_dir
        .join("working-copies")
        .join(working_copy.to_string())
        .join(WORKSPACES_DIR)
        .join(view_name)
}

fn ensure_working_copy_workspace_dir(
    dot_dir: &Path,
    working_copy: WorkingCopyId,
    view_name: &str,
) -> Result<(), RepositoryError> {
    std::fs::create_dir_all(working_copy_workspace_path(
        dot_dir,
        working_copy,
        view_name,
    ))?;
    Ok(())
}

/// Remove empty ancestor directories after file removal.
///
/// Given an iterator of relative paths that were just deleted, this
/// collects every parent directory, sorts them deepest-first, and
/// attempts `std::fs::remove_dir` on each.  Because `remove_dir` only
/// succeeds on *empty* directories, this is always safe — a directory
/// that still contains files (tracked, untracked, or otherwise) will
/// simply fail silently.
///
/// Extracting this into a standalone helper keeps `switch_view` at the
/// orchestration level and makes the cleanup logic reusable for other
/// operations (e.g. `atomic clean`).
fn cleanup_empty_ancestors<'a>(
    _working_copy: WorkingCopyId,
    root: &Path,
    removed_paths: impl Iterator<Item = &'a str>,
) {
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    for path in removed_paths {
        let p = PathBuf::from(path);
        let mut ancestor = p.parent();
        while let Some(dir) = ancestor {
            if dir == Path::new("") || dir == Path::new(".") {
                break;
            }
            dirs.insert(dir.to_path_buf());
            ancestor = dir.parent();
        }
    }
    // Sort deepest-first so children are removed before parents.
    let mut sorted: Vec<PathBuf> = dirs.into_iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.components().count()));
    for dir in sorted {
        let abs = root.join(&dir);
        if abs.is_dir() {
            // Only succeeds if the directory is empty — safe by construction.
            let _ = std::fs::remove_dir(&abs);
        }
    }
}

impl Repository {
    /// Switch to a different view and update the working copy.
    ///
    /// This is the primary method for switching views. It:
    /// 1. Validates the view exists
    /// 2. Updates the current view pointer
    /// 3. Materializes the working copy to match the new view's state
    ///
    /// # Arguments
    ///
    /// * `working_copy` - The validated physical working-copy identity
    /// * `view` - The name of the view to switch to
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation (files written, etc.)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view does not exist
    /// - The working copy cannot be updated
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut repo = Repository::open(".")?;
    ///
    /// // Switch to feature view and update working copy
    /// let result = repo.switch_view(working_copy, "feature")?;
    /// println!("Updated {} files", result.files_written);
    /// ```
    pub fn switch_view(
        &mut self,
        working_copy: WorkingCopyId,
        view: &str,
    ) -> Result<MaterializeResult, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let old_view_name = self.desired_view_name(working_copy)?;

        // Resolve both views and validate both dependency closures before the
        // switch publishes a pointer or mutates TREE-derived state.
        let (old_files, new_files, new_visibility, new_view_id) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let old_view = txn
                .get_view(&old_view_name)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: old_view_name.clone(),
                })?;
            let new_view = txn
                .get_view(view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view.to_string(),
                })?;

            let old_membership = view_membership(&txn, &old_view)?;
            let new_membership = view_membership(&txn, &new_view)?;
            let old_visibility = graph_visibility_from_membership(&txn, &old_membership)?;
            let new_visibility = graph_visibility_from_membership(&txn, &new_membership)?;
            let old_projection = self.project_tree_for_visibility(&txn, &old_visibility)?;
            let new_projection = self.project_tree_for_visibility(&txn, &new_visibility)?;
            let old_files: HashSet<String> = old_projection
                .present
                .into_iter()
                .filter_map(|(path, item)| (!item.is_directory).then_some(path))
                .collect();
            let new_files: HashSet<String> = new_projection
                .present
                .into_iter()
                .filter_map(|(path, item)| (!item.is_directory).then_some(path))
                .collect();

            (old_files, new_files, new_visibility, new_view.id)
        };

        // Render the complete target before publishing the target pointer,
        // shelving ignored files, or removing tracked paths. This does not make
        // filesystem execution crash-safe, but it guarantees graph/preload/
        // content errors refuse the switch before its first external effect.
        self.validate_materialization_with_visibility(
            working_copy,
            None,
            new_visibility.clone(),
            new_view_id,
        )?;

        // Apply only the small set of view-scoped TREE operations and publish
        // the new pointer while holding the same database write lock. A marker
        // makes the transition recoverable if the process exits mid-switch.
        self.align_to_view(working_copy, view)?;

        if std::env::var_os("ATOMIC_TRACE_SWITCH").is_some() {
            eprintln!("[switch] {} -> {}", old_view_name, view);
            eprintln!(
                "[switch] old_files={} new_files={}",
                old_files.len(),
                new_files.len()
            );
            for f in old_files.difference(&new_files) {
                eprintln!("[switch] REMOVE (old only): {}", f);
            }
            for f in new_files.difference(&old_files) {
                eprintln!("[switch] ADD (new only): {}", f);
            }
        }

        let filesystem_working_copy = FileSystem::from_root(&self.root);

        // ── Phase 1: Shelve ignored files into the OLD view's workspace ──
        //
        // All ignored files are shelved per-view EXCEPT paths listed in
        // `[workspace] expose` in `.atomic/config.toml`.  Exposed paths
        // persist across all views (tool configs like .opencode/, .vscode/).
        //
        // This uses `rename()` which is O(1) on the same filesystem —
        // no data is copied, just inode pointers are updated.
        //
        // The rule:
        //   - Tracked files      → managed by the graph (phases 2-4)
        //   - Untracked, ignored, exposed  → left alone (persists across views)
        //   - Untracked, ignored, NOT exposed → shelved/restored per-view (phases 1 & 5)
        //   - Untracked, novel   → user's undecided work, left alone
        let old_ws = working_copy_workspace_path(&self.dot_dir, working_copy, &old_view_name);
        ensure_working_copy_workspace_dir(&self.dot_dir, working_copy, &old_view_name)?;

        let repo_expose = atomic_config::RepoConfig::load(&self.config_path())
            .unwrap_or_default()
            .workspace
            .expose;
        let global_expose = atomic_config::GlobalConfig::load()
            .map(|c| c.workspace.expose)
            .unwrap_or_default();

        // Merge global + repo-local expose patterns (deduplicated)
        let mut expose_patterns = global_expose;
        for p in repo_expose {
            if !expose_patterns.contains(&p) {
                expose_patterns.push(p);
            }
        }

        // Collect tracked file paths so we never shelve them — tracked
        // files are managed by the graph (phases 2–4), not by shelving.
        let tracked_paths: HashSet<String> = old_files.union(&new_files).cloned().collect();

        let ignored_paths: Vec<String> = self
            .collect_ignored_paths_on_disk()
            .into_iter()
            .filter(|path| {
                // Never shelve tracked files — they belong to the graph
                if tracked_paths.contains(path) {
                    return false;
                }
                // Never shelve files under a tracked directory
                if tracked_paths
                    .iter()
                    .any(|t| t.starts_with(&format!("{}/", path)))
                {
                    return false;
                }
                // Keep paths that are NOT exposed (those get shelved)
                !expose_patterns
                    .iter()
                    .any(|pattern| path == pattern || path.starts_with(&format!("{}/", pattern)))
            })
            .collect();
        if !ignored_paths.is_empty() {
            // Clear old workspace content, then move current ignored files in.
            // We clear first because the workspace may contain stale state
            // from a previous shelve.
            for path in &ignored_paths {
                let ws_dest = old_ws.join(path);
                // Remove stale entry in workspace if it exists
                if ws_dest.is_dir() {
                    std::fs::remove_dir_all(&ws_dest)?;
                } else if ws_dest.exists() {
                    std::fs::remove_file(&ws_dest)?;
                }
                // Ensure parent dirs exist in workspace
                if let Some(parent) = ws_dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // Move from working copy → workspace (O(1) rename)
                let src = self.root.join(path);
                if src.exists() {
                    std::fs::rename(&src, &ws_dest)?;
                }
            }
        }

        // ── Phase 2: Remove tracked files that belong to the old view ──
        //
        // Files visible on the old view but NOT on the new view are
        // removed from disk.
        let mut removed_paths: Vec<String> = Vec::new();
        let mut paths_to_remove: Vec<String> = old_files.difference(&new_files).cloned().collect();
        paths_to_remove.sort();
        for path in paths_to_remove {
            let abs_path = self.root.join(&path);
            if abs_path.exists()
                && !abs_path.is_dir()
                && filesystem_working_copy.remove_path(&path, false).is_ok()
            {
                removed_paths.push(path);

                // Deterministic expected-red failpoint for the bridge recovery
                // contract. Release builds contain no environment-variable
                // branch; the operation journal will eventually recover this
                // deliberately partial transition.
                #[cfg(debug_assertions)]
                if removed_paths.len() == 1
                    && std::env::var_os("ATOMIC_FAIL_SWITCH_AFTER_FIRST_TRACKED_REMOVAL").is_some()
                {
                    return Err(RepositoryError::Io(std::io::Error::other(
                        "debug failpoint: switch failed after first tracked-path removal",
                    )));
                }
            }
        }

        // ── Phase 3: Clean up empty ancestor directories ────────────────
        let all_removed = removed_paths
            .iter()
            .map(|s| s.as_str())
            .chain(ignored_paths.iter().map(|s| s.as_str()));
        cleanup_empty_ancestors(working_copy, &self.root, all_removed);

        // ── Phase 4: Materialize the new view's tracked files from graph ─
        //
        // Run a complete target materialization. Scoped FILE_INDEX entries let
        // unchanged files skip writes while still proving the whole desired view
        // was output successfully before its materialized state is recorded.
        let result = self.materialize_parallel_with_visibility(
            working_copy,
            None,
            new_visibility.clone(),
            new_view_id,
        )?;

        // ── Phase 5: Restore ignored files from the NEW view's workspace ─
        //
        // Move artifacts from the working-copy-scoped workspace back into the
        // working copy. Again O(1) renames, no data copying.
        let new_ws = working_copy_workspace_path(&self.dot_dir, working_copy, view);
        if new_ws.is_dir() {
            self.restore_workspace_to_working_copy(&new_ws)?;
        }

        self.mark_working_copy_materialized(working_copy, view)?;
        Ok(result)
    }

    /// Restore entries from a workspace directory into the working copy.
    ///
    /// Walks the top-level entries in `ws_dir` and moves each into the
    /// project root via `rename()`.  Skips the `.atomic` directory if
    /// present.
    fn restore_workspace_to_working_copy(&self, ws_dir: &Path) -> Result<(), RepositoryError> {
        for entry in std::fs::read_dir(ws_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Never move VCS administrative directories into the working copy.
            // `.git` belongs to Git's checkout and must not be view-scoped.
            if name_str == DOT_DIR || name_str == ".git" {
                continue;
            }

            let src = entry.path();
            let dst = self.root.join(&*name_str);

            // If the destination already exists (e.g. a directory that
            // was created by materialize for tracked content),
            // merge by recursing into it rather than replacing it.
            if dst.is_dir() && src.is_dir() {
                self.merge_dir_into(&src, &dst)?;
                std::fs::remove_dir_all(&src)?;
            } else {
                // Ensure parent exists
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&src, &dst)?;
            }
        }
        Ok(())
    }

    /// Recursively merge the contents of `src_dir` into `dst_dir`.
    ///
    /// Files in `src_dir` are moved into `dst_dir`.  If a subdirectory
    /// exists in both, the merge recurses.  This is used when restoring
    /// workspace artifacts into a directory that already contains tracked
    /// files (e.g. `src/` might have tracked `.ts` files from the graph
    /// AND ignored `.cache/` from the workspace).
    fn merge_dir_into(&self, src_dir: &Path, dst_dir: &Path) -> Result<(), RepositoryError> {
        for entry in std::fs::read_dir(src_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let src = entry.path();
            let dst = dst_dir.join(&name);

            if dst.is_dir() && src.is_dir() {
                self.merge_dir_into(&src, &dst)?;
                std::fs::remove_dir_all(&src)?;
            } else {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::rename(&src, &dst)?;
            }
        }
        Ok(())
    }

    /// Walk the working copy and collect relative paths of files and
    /// directories that match `.atomicignore` rules.
    ///
    /// Only top-level ignored entries are returned — if `node_modules/`
    /// matches, we return `"node_modules"` rather than enumerating every
    /// file inside it (the caller will `remove_dir_all`).
    ///
    /// Paths that live inside `.atomic/` are never returned.
    fn collect_ignored_paths_on_disk(&self) -> Vec<String> {
        let rules = self.ignore_rules();
        let mut result = Vec::new();

        // Recursive walker that stops descending into ignored directories.
        fn walk(
            root: &Path,
            dir: &Path,
            rules: &crate::ignore::IgnoreRules,
            out: &mut Vec<String>,
        ) {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let abs = entry.path();
                let rel = match abs.strip_prefix(root) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                // Never touch VCS administrative directories. `.git` can be
                // ignored by `.atomicignore` after `atomic git import`, but
                // it remains owned by Git rather than a view workspace.
                if rel.starts_with(DOT_DIR) || rel.starts_with(".git") {
                    continue;
                }

                let is_dir = abs.is_dir();

                if rules.is_ignored(rel, is_dir) {
                    // Collect the top-level ignored entry — don't recurse.
                    if let Some(s) = rel.to_str() {
                        out.push(s.to_string());
                    }
                } else if is_dir {
                    // Not ignored — recurse to find ignored children.
                    walk(root, &abs, rules, out);
                }
                // Non-ignored files are left alone.
            }
        }

        walk(&self.root, &self.root, &rules, &mut result);
        result
    }
}

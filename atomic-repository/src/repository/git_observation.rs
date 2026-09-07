//! Read-only Git index and physical worktree observation for CB-4B.

use super::project_tree::{git_object_id, GitObjectKind};
use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use atomic_core::operation::{GitHashAlgorithm, GitObjectId};
use atomic_objects::content_key;
use thiserror::Error;
use walkdir::WalkDir;

use crate::content_filter::ContentFilter;

const ASSUME_VALID: u16 = 0x8000;
const INTENT_TO_ADD: u16 = 0x2000;
const SKIP_WORKTREE: u16 = 0x4000;

/// Fail-closed errors produced before an observation can be trusted.
#[derive(Debug, Error)]
pub enum ObservationError {
    #[error("cannot open Git repository at '{path}': {message}")]
    GitOpen { path: String, message: String },
    #[error("cannot read Git index: {0}")]
    GitIndex(String),
    #[error("Git object format '{observed}' does not match policy {expected:?}")]
    ObjectFormat {
        observed: String,
        expected: GitHashAlgorithm,
    },
    #[error("Git index object ID width is unsupported for {0:?}")]
    UnsupportedObjectFormat(GitHashAlgorithm),
    #[error("invalid raw repository path: {0}")]
    InvalidPath(String),
    #[error("cannot inspect worktree path '{path}': {message}")]
    WorktreeIo { path: String, message: String },
    #[error("content clean failed for '{path}': {message}")]
    Filter { path: String, message: String },
    #[error("platform cannot represent lossless Unix repository paths")]
    UnsupportedPlatform,
    #[error("observed platform capabilities differ from conversion policy: expected {expected}, observed {observed}")]
    PlatformMismatch { expected: String, observed: String },
    #[error("cannot compute index tree: {0}")]
    IndexTree(String),
}

/// Read a Git index without refreshing, locking, or writing it.
pub fn observe_git_index(
    root: &Path,
    policy: &ConversionPolicy,
) -> Result<GitIndexState, ObservationError> {
    let repository =
        git2::Repository::discover(root).map_err(|error| ObservationError::GitOpen {
            path: root.display().to_string(),
            message: error.to_string(),
        })?;
    let observed_algorithm = repository_object_algorithm(&repository)?;
    if observed_algorithm != policy.object_format {
        return Err(ObservationError::ObjectFormat {
            observed: format!("{observed_algorithm:?}"),
            expected: policy.object_format,
        });
    }
    let index = repository
        .index()
        .map_err(|error| ObservationError::GitIndex(error.to_string()))?;
    let mut entries = Vec::with_capacity(index.len());
    for entry in index.iter() {
        let path = RepoPath::from_bytes(&entry.path)
            .map_err(|error| ObservationError::InvalidPath(error.to_string()))?;
        let oid = GitObjectId::new(observed_algorithm, entry.id.as_bytes().to_vec())
            .map_err(|_| ObservationError::UnsupportedObjectFormat(observed_algorithm))?;
        entries.push(GitIndexEntry {
            path,
            stage: ((entry.flags >> 12) & 0x3) as u8,
            mode: canonical_index_mode(entry.mode),
            oid: Some(oid),
            intent_to_add: entry.flags_extended & INTENT_TO_ADD != 0,
            skip_worktree: entry.flags_extended & SKIP_WORKTREE != 0,
            assume_unchanged: entry.flags & ASSUME_VALID != 0,
            sparse_directory: entry.mode & 0o170000 == 0o040000,
        });
    }
    entries.sort_by(|left, right| {
        (&left.path, left.stage, &left.oid).cmp(&(&right.path, right.stage, &right.oid))
    });
    let tree = compute_index_tree(observed_algorithm, &entries)?;
    Ok(GitIndexState {
        version: GIT_INDEX_STATE_VERSION,
        index_version: index.version(),
        object_format: observed_algorithm,
        entries,
        tree,
    })
}

fn repository_object_algorithm(
    repository: &git2::Repository,
) -> Result<GitHashAlgorithm, ObservationError> {
    let config = repository
        .config()
        .map_err(|error| ObservationError::GitIndex(error.to_string()))?;
    let value = match config.get_string("extensions.objectFormat") {
        Ok(value) => value,
        Err(error) if error.code() == git2::ErrorCode::NotFound => "sha1".to_string(),
        Err(error) => return Err(ObservationError::GitIndex(error.to_string())),
    };
    match value.to_ascii_lowercase().as_str() {
        "sha1" => Ok(GitHashAlgorithm::Sha1),
        "sha256" => Ok(GitHashAlgorithm::Sha256),
        _ => Err(ObservationError::ObjectFormat {
            observed: value,
            expected: GitHashAlgorithm::Sha1,
        }),
    }
}

fn canonical_index_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o040000 => 0o040000,
        0o100000 if mode & 0o111 == 0 => 0o100644,
        0o100000 => 0o100755,
        0o120000 => 0o120000,
        0o160000 => 0o160000,
        _ => mode,
    }
}

#[derive(Default)]
struct IndexDirectory {
    entries: BTreeMap<Vec<u8>, IndexNode>,
}

enum IndexNode {
    Directory(IndexDirectory),
    Object { mode: u32, oid: GitObjectId },
}

pub(super) fn compute_index_tree(
    algorithm: GitHashAlgorithm,
    entries: &[GitIndexEntry],
) -> Result<Option<GitObjectId>, ObservationError> {
    if entries.iter().any(|entry| {
        entry.stage != 0
            || entry.intent_to_add
            || entry.oid.is_none()
            || entry
                .oid
                .as_ref()
                .is_some_and(|oid| oid.as_bytes().iter().all(|byte| *byte == 0))
    }) {
        return Ok(None);
    }
    let mut root = IndexDirectory::default();
    for entry in entries {
        let oid = entry.oid.clone().expect("checked above");
        if oid.algorithm() != algorithm {
            return Err(ObservationError::UnsupportedObjectFormat(algorithm));
        }
        insert_index_entry(
            &mut root,
            entry.path.components().collect(),
            entry.mode,
            oid,
        )?;
    }
    hash_index_directory(algorithm, &root).map(Some)
}

fn insert_index_entry(
    root: &mut IndexDirectory,
    components: Vec<&[u8]>,
    mode: u32,
    oid: GitObjectId,
) -> Result<(), ObservationError> {
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| ObservationError::IndexTree("empty path".into()))?;
    let mut directory = root;
    for parent in parents {
        let node = directory
            .entries
            .entry(parent.to_vec())
            .or_insert_with(|| IndexNode::Directory(IndexDirectory::default()));
        match node {
            IndexNode::Directory(child) => directory = child,
            IndexNode::Object { .. } => {
                return Err(ObservationError::IndexTree(
                    "file/directory path collision".into(),
                ))
            }
        }
    }
    let node = if mode == 0o040000 {
        IndexNode::Object { mode, oid }
    } else if matches!(mode, 0o100644 | 0o100755 | 0o120000 | 0o160000) {
        IndexNode::Object { mode, oid }
    } else {
        return Err(ObservationError::IndexTree(format!(
            "unsupported mode {mode:#o}"
        )));
    };
    if directory.entries.insert(name.to_vec(), node).is_some() {
        return Err(ObservationError::IndexTree("duplicate path".into()));
    }
    Ok(())
}

fn hash_index_directory(
    algorithm: GitHashAlgorithm,
    directory: &IndexDirectory,
) -> Result<GitObjectId, ObservationError> {
    struct Encoded {
        name: Vec<u8>,
        mode: u32,
        oid: GitObjectId,
    }
    let mut encoded = Vec::new();
    for (name, node) in &directory.entries {
        match node {
            IndexNode::Directory(child) => encoded.push(Encoded {
                name: name.clone(),
                mode: 0o040000,
                oid: hash_index_directory(algorithm, child)?,
            }),
            IndexNode::Object { mode, oid } => encoded.push(Encoded {
                name: name.clone(),
                mode: *mode,
                oid: oid.clone(),
            }),
        }
    }
    encoded.sort_by(|left, right| {
        git_name_order(
            &left.name,
            left.mode == 0o040000,
            &right.name,
            right.mode == 0o040000,
        )
    });
    let mut bytes = Vec::new();
    for entry in encoded {
        bytes.extend_from_slice(format!("{:o} ", entry.mode).as_bytes());
        bytes.extend_from_slice(&entry.name);
        bytes.push(0);
        bytes.extend_from_slice(entry.oid.as_bytes());
    }
    git_object_id(algorithm, GitObjectKind::Tree, &bytes)
        .map_err(|error| ObservationError::IndexTree(error.to_string()))
}

fn git_name_order(
    left: &[u8],
    left_tree: bool,
    right: &[u8],
    right_tree: bool,
) -> std::cmp::Ordering {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    if left_tree {
        left.push(b'/');
    }
    if right_tree {
        right.push(b'/');
    }
    left.cmp(&right)
}

/// Observe physical worktree state without following symlinks.
///
/// Gitlink directories are retained as single explicit entries when identified
/// by the index; their contents are never traversed.
pub fn observe_worktree(
    root: &Path,
    index: Option<&GitIndexState>,
    filter: &dyn ContentFilter,
    policy: &ConversionPolicy,
) -> Result<WorktreeObservation, ObservationError> {
    if !cfg!(unix) || !policy.platform.lossless_unix_paths {
        return Err(ObservationError::UnsupportedPlatform);
    }
    let platform = detect_platform_capabilities(root, policy)?;
    if platform != policy.platform {
        return Err(ObservationError::PlatformMismatch {
            expected: format!("{:?}", policy.platform),
            observed: format!("{platform:?}"),
        });
    }
    let stage_zero: BTreeMap<RepoPath, &GitIndexEntry> = index
        .into_iter()
        .flat_map(|state| state.entries.iter())
        .filter(|entry| entry.stage == 0)
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let gitlinks: BTreeSet<RepoPath> = stage_zero
        .iter()
        .filter(|(_, entry)| entry.mode == 0o160000)
        .map(|(path, _)| path.clone())
        .collect();
    let mut entries = Vec::new();
    let mut walker = WalkDir::new(root).follow_links(false).into_iter();
    while let Some(next) = walker.next() {
        let entry = next.map_err(|error| ObservationError::WorktreeIo {
            path: error.path().unwrap_or(root).display().to_string(),
            message: error.to_string(),
        })?;
        if entry.path() == root {
            continue;
        }
        let relative =
            entry
                .path()
                .strip_prefix(root)
                .map_err(|error| ObservationError::WorktreeIo {
                    path: entry.path().display().to_string(),
                    message: error.to_string(),
                })?;
        let path = RepoPath::from_native(relative)
            .map_err(|error| ObservationError::InvalidPath(error.to_string()))?;
        let first = path.components().next().unwrap_or_default();
        if first == b".git" || first == b".atomic" {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| ObservationError::WorktreeIo {
                path: path.escaped(),
                message: error.to_string(),
            })?;
        let is_gitlink = gitlinks.contains(&path);
        if metadata.is_dir() && !is_gitlink {
            continue;
        }
        if is_gitlink {
            walker.skip_current_dir();
        }
        let physical_kind = if metadata.file_type().is_symlink() {
            PhysicalKind::Symlink
        } else if metadata.is_file() {
            PhysicalKind::Regular
        } else if metadata.is_dir() {
            PhysicalKind::Directory
        } else {
            PhysicalKind::Other
        };
        let worktree_bytes = match physical_kind {
            PhysicalKind::Regular => {
                fs::read(entry.path()).map_err(|error| ObservationError::WorktreeIo {
                    path: path.escaped(),
                    message: error.to_string(),
                })?
            }
            PhysicalKind::Symlink => symlink_target_bytes(entry.path(), &path)?,
            PhysicalKind::Directory | PhysicalKind::Other => Vec::new(),
        };
        let (repository_bytes_after_clean, filter_warnings) = match physical_kind {
            PhysicalKind::Regular => {
                let filtered = filter.clean(relative, &worktree_bytes).map_err(|error| {
                    ObservationError::Filter {
                        path: path.escaped(),
                        message: error.to_string(),
                    }
                })?;
                (Some(filtered.bytes), filtered.warnings)
            }
            PhysicalKind::Symlink => (Some(worktree_bytes.clone()), Vec::new()),
            PhysicalKind::Directory | PhysicalKind::Other => (None, Vec::new()),
        };
        let repository_content_after_clean =
            repository_bytes_after_clean.as_deref().map(content_key);
        let disposition = policy
            .exclusions
            .exclusion(&path)
            .map(ManifestDisposition::Excluded)
            .unwrap_or(ManifestDisposition::Included);
        let indexed = stage_zero.get(&path).copied();
        let repository_kind = indexed.and_then(|entry| match entry.mode {
            0o100644 | 0o100755 => Some(atomic_core::change::InodeKind::Regular),
            0o120000 => Some(atomic_core::change::InodeKind::Symlink),
            0o160000 => Some(atomic_core::change::InodeKind::Gitlink),
            _ => None,
        });
        let gitlink = indexed
            .filter(|entry| entry.mode == 0o160000)
            .and_then(|entry| entry.oid.clone());
        entries.push(WorktreeEntry {
            path,
            physical_kind,
            repository_kind,
            gitlink,
            mode: unix_mode(&metadata),
            size: metadata.len(),
            worktree_content: content_key(&worktree_bytes),
            worktree_bytes,
            repository_bytes_after_clean,
            repository_content_after_clean,
            disposition,
            filter_warnings,
            filter_error: None,
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(WorktreeObservation::new(platform, entries))
}

#[cfg(unix)]
fn symlink_target_bytes(path: &Path, repo_path: &RepoPath) -> Result<Vec<u8>, ObservationError> {
    use std::os::unix::ffi::OsStrExt;
    fs::read_link(path)
        .map(|target| target.as_os_str().as_bytes().to_vec())
        .map_err(|error| ObservationError::WorktreeIo {
            path: repo_path.escaped(),
            message: error.to_string(),
        })
}

#[cfg(not(unix))]
fn symlink_target_bytes(_path: &Path, _repo_path: &RepoPath) -> Result<Vec<u8>, ObservationError> {
    Err(ObservationError::UnsupportedPlatform)
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> Option<u16> {
    use std::os::unix::fs::PermissionsExt;
    Some((metadata.permissions().mode() & 0o777) as u16)
}

#[cfg(not(unix))]
fn unix_mode(_metadata: &fs::Metadata) -> Option<u16> {
    None
}

fn detect_platform_capabilities(
    root: &Path,
    policy: &ConversionPolicy,
) -> Result<PlatformCapabilities, ObservationError> {
    let mut capabilities = policy.platform.clone();
    capabilities.lossless_unix_paths = cfg!(unix);
    if let Ok(repository) = git2::Repository::discover(root) {
        let config = repository
            .config()
            .map_err(|error| ObservationError::GitIndex(error.to_string()))?;
        capabilities.executable_bit = config.get_bool("core.filemode").unwrap_or(cfg!(unix));
        capabilities.symlinks = config.get_bool("core.symlinks").unwrap_or(cfg!(unix));
        capabilities.case_sensitive = !config.get_bool("core.ignorecase").unwrap_or(false);
        capabilities.unicode_normalizing =
            config.get_bool("core.precomposeunicode").unwrap_or(false);
    }
    Ok(capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_tree_is_unavailable_for_conflicts_and_intent_to_add() {
        let oid = GitObjectId::new(GitHashAlgorithm::Sha1, vec![1; 20]).unwrap();
        let base = GitIndexEntry {
            path: RepoPath::from_bytes(b"file").unwrap(),
            stage: 2,
            mode: 0o100644,
            oid: Some(oid),
            intent_to_add: false,
            skip_worktree: false,
            assume_unchanged: false,
            sparse_directory: false,
        };
        assert!(compute_index_tree(GitHashAlgorithm::Sha1, &[base.clone()])
            .unwrap()
            .is_none());
        let mut intent = base;
        intent.stage = 0;
        intent.intent_to_add = true;
        assert!(compute_index_tree(GitHashAlgorithm::Sha1, &[intent])
            .unwrap()
            .is_none());
    }
}

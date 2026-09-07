//! The shadow-publication pipeline and its CB-4B equivalence capability.
//!
//! `atomic git push`, native push with an active bridge, and shadow projection
//! build an Atomic `ProjectTree`, observe Git index/worktree state read-only,
//! and require an equivalent report before any publication mutation. The
//! already-equivalent index supplies the tree; this module never stages an
//! unchecked candidate merely to make validation pass.
//!
//! Validator rules enforced here (pre-commit):
//! - **V1** — no unresolved conflict markers (shares `record`'s detector).
//! - **V4** — no git-excluded provenance path (`.atomic/`, `.vault/`,
//!   `.atomicignore`) is ever staged.
//!
//! V2 (tree↔view coherence) is the CB-4B joint report. V3 remains the Git
//! history/Atomic-state lineage check in `git::push`.

use std::io::IsTerminal;
use std::path::Path;

use git2::Repository as GitRepository;

use atomic_config::ContentFilterConfig;
use atomic_core::operation::GitHashAlgorithm;
use atomic_objects::content_key;
use atomic_repository::{
    compare_project_state, observe_git_index, observe_worktree, ConversionPolicy,
    EquivalenceClaims, GitAttributesFilter, ManifestRoot, Repository,
};

use crate::error::{CliError, CliResult};
use crate::output::{print_info, print_warning};

/// Marker handling is explicit at the publication boundary; callers cannot
/// accidentally smuggle an unchecked boolean into the validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictMarkerPolicy {
    Refuse,
    AllowExplicitly,
}

/// Private capability proving that the Atomic project tree, Git index, and
/// physical worktree were equivalent under one concrete conversion policy.
///
/// Construction is intentionally restricted to [`verify_git_publication`].
/// Every publication mutation path requires this value and re-observes its
/// leases immediately before its final ref/network effect.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedPublication {
    view: String,
    atomic_state: String,
    policy: ConversionPolicy,
    manifest_root: ManifestRoot,
    index_root: ManifestRoot,
    worktree_root: ManifestRoot,
    git_tree: atomic_core::operation::GitObjectId,
    git_head: Option<git2::Oid>,
}

impl VerifiedPublication {
    pub(crate) fn view(&self) -> &str {
        &self.view
    }

    pub(crate) fn atomic_state(&self) -> &str {
        &self.atomic_state
    }

    pub(crate) fn manifest_root(&self) -> &ManifestRoot {
        &self.manifest_root
    }

    pub(crate) fn policy_root(&self) -> ManifestRoot {
        self.policy.root()
    }

    pub(crate) fn object_algorithm(&self) -> GitHashAlgorithm {
        self.policy.object_format
    }

    pub(crate) fn git_tree(&self) -> &atomic_core::operation::GitObjectId {
        &self.git_tree
    }

    pub(crate) fn git_tree_oid(&self) -> CliResult<git2::Oid> {
        if self.git_tree.algorithm() != GitHashAlgorithm::Sha1 {
            return Err(git_error(format!(
                "Git publication through libgit2 does not support {:?} repositories",
                self.git_tree.algorithm()
            )));
        }
        git2::Oid::from_bytes(self.git_tree.as_bytes()).map_err(|error| {
            git_error(format!(
                "verified Git tree has an invalid object identity: {error}"
            ))
        })
    }

    pub(crate) fn bind_committed_head(
        &mut self,
        git_repo: &GitRepository,
        commit_oid: git2::Oid,
    ) -> CliResult<()> {
        let commit = git_repo.find_commit(commit_oid).map_err(|error| {
            git_error(format!(
                "cannot bind published Git commit {commit_oid}: {error}"
            ))
        })?;
        if commit.tree_id() != self.git_tree_oid()? {
            return Err(git_error(
                "refusing to bind a Git commit whose tree differs from the verified project tree",
            ));
        }
        let observed = git_repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .ok_or_else(|| {
                git_error("cannot bind publication to an unborn or symbolic-only HEAD")
            })?;
        if observed != commit_oid {
            return Err(git_error(format!(
                "Git HEAD changed while binding publication: expected {commit_oid}, found {observed}"
            )));
        }
        self.git_head = Some(commit_oid);
        Ok(())
    }

    pub(crate) fn reobserve_before_commit(
        &self,
        repo: &Repository,
        repo_root: &Path,
        git_repo: &GitRepository,
    ) -> CliResult<()> {
        self.reobserve(repo, repo_root)?;
        let observed = git_repo.head().ok().and_then(|head| head.target());
        if observed != self.git_head {
            return Err(git_error(format!(
                "Git HEAD lease changed before commit: expected {:?}, found {:?}",
                self.git_head, observed
            )));
        }
        Ok(())
    }

    pub(crate) fn reobserve_for_git_push(
        &self,
        repo: &Repository,
        repo_root: &Path,
        git_repo: &GitRepository,
    ) -> CliResult<()> {
        self.reobserve(repo, repo_root)?;
        let expected = self
            .git_head
            .ok_or_else(|| git_error("cannot publish an unborn Git HEAD"))?;
        let head = git_repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| git_error(format!("cannot re-observe Git HEAD: {error}")))?;
        if head.id() != expected || head.tree_id() != self.git_tree_oid()? {
            return Err(git_error(format!(
                "Git HEAD lease changed before push: expected commit {expected} with tree {}, found commit {} with tree {}",
                self.git_tree_oid()?,
                head.id(),
                head.tree_id()
            )));
        }
        Ok(())
    }

    /// Re-observe every lease represented by this capability. This rejects an
    /// Atomic view/state change, policy change, index edit, or worktree edit.
    pub(crate) fn reobserve(&self, repo: &Repository, repo_root: &Path) -> CliResult<()> {
        let current = verify_git_publication(repo, repo_root, &self.view)?;
        if current.policy != self.policy
            || current.atomic_state != self.atomic_state
            || current.manifest_root != self.manifest_root
            || current.index_root != self.index_root
            || current.worktree_root != self.worktree_root
            || current.git_tree != self.git_tree
        {
            return Err(git_error(
                "publication state changed after equivalence verification; retry from fresh observations",
            ));
        }
        Ok(())
    }
}

/// Verify a mandatory CB-4B publication gate for a Git-backed command.
pub(crate) fn verify_git_publication(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<VerifiedPublication> {
    let git_repo = GitRepository::discover(repo_root)
        .map_err(|error| git_error(format!("cannot discover Git repository: {error}")))?;
    let (policy, filters) = current_conversion_policy(repo_root, &git_repo)?;
    verify_git_publication_with_policy(repo, repo_root, view, policy, filters)
}

/// Whether native publication must run the Git bridge gate.
pub(crate) fn bridge_publication_required(repo_root: &Path) -> bool {
    GitRepository::discover(repo_root)
        .map(|repository| shadow_sync_active(&repository))
        .unwrap_or(false)
}

/// Native Atomic publication is unchanged without an active Git shadow. Once
/// the shadow marker exists, however, publication is gated fail-closed.
pub(crate) fn verify_bridge_publication(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<Option<VerifiedPublication>> {
    let Ok(git_repo) = GitRepository::discover(repo_root) else {
        return Ok(None);
    };
    if !shadow_sync_active(&git_repo) {
        return Ok(None);
    }
    let (policy, filters) = current_conversion_policy(repo_root, &git_repo)?;
    verify_git_publication_with_policy(repo, repo_root, view, policy, filters).map(Some)
}

fn verify_git_publication_with_policy(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
    policy: ConversionPolicy,
    filters: ContentFilterConfig,
) -> CliResult<VerifiedPublication> {
    let project = repo.project_tree(view, &policy).map_err(|error| {
        git_error(format!(
            "cannot build Atomic publication project tree: {error}"
        ))
    })?;
    let filter = GitAttributesFilter::new(repo_root, filters);
    let index = observe_git_index(repo_root, &policy)
        .map_err(|error| git_error(format!("cannot observe Git index: {error}")))?;
    let worktree = observe_worktree(repo_root, Some(&index), &filter, &policy)
        .map_err(|error| git_error(format!("cannot observe Git worktree: {error}")))?;
    let claims = EquivalenceClaims {
        manifest_version: Some(project.manifest.version),
        object_algorithm: Some(project.git.algorithm),
        manifest_root: Some(project.manifest.root().content_key.clone()),
        conversion_policy_root: Some(policy.root().content_key),
        git_tree_root: Some(project.git.root.clone()),
    };
    let report = compare_project_state(&project, &index, &worktree, &policy, &claims);
    if !report.is_equivalent() {
        let details = report
            .mismatches
            .iter()
            .take(8)
            .map(|mismatch| {
                let path = mismatch
                    .path
                    .as_ref()
                    .map(|path| format!(" at '{}'", path.escaped()))
                    .unwrap_or_default();
                format!(
                    "{:?}/{:?}{path}: expected {}, observed {}",
                    mismatch.layer, mismatch.kind, mismatch.expected, mismatch.actual
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(git_error(format!(
            "CB-4B publication equivalence failed for view '{view}': {details}. No publication mutation was attempted"
        )));
    }
    let atomic_state = repo
        .get_view_info(view)
        .map_err(CliError::Repository)?
        .state_base32();
    let git_head = GitRepository::discover(repo_root)
        .ok()
        .and_then(|repository| repository.head().ok().and_then(|head| head.target()));
    Ok(VerifiedPublication {
        view: view.to_string(),
        atomic_state,
        policy,
        manifest_root: project.manifest.root(),
        index_root: index.root(),
        worktree_root: worktree.root(),
        git_tree: project.git.root,
        git_head,
    })
}

fn current_conversion_policy(
    repo_root: &Path,
    git_repo: &GitRepository,
) -> CliResult<(ConversionPolicy, ContentFilterConfig)> {
    let config = git_repo
        .config()
        .map_err(|error| git_error(format!("cannot read Git configuration: {error}")))?;
    let object_format = match config.get_string("extensions.objectFormat") {
        Ok(value) if value.eq_ignore_ascii_case("sha1") => GitHashAlgorithm::Sha1,
        Ok(value) if value.eq_ignore_ascii_case("sha256") => GitHashAlgorithm::Sha256,
        Ok(value) => {
            return Err(git_error(format!(
                "unsupported Git object algorithm '{value}'"
            )))
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => GitHashAlgorithm::Sha1,
        Err(error) => {
            return Err(git_error(format!(
                "cannot read Git object algorithm: {error}"
            )))
        }
    };
    let mut policy = ConversionPolicy::new(object_format);
    policy.platform.executable_bit = config.get_bool("core.filemode").unwrap_or(cfg!(unix));
    policy.platform.symlinks = config.get_bool("core.symlinks").unwrap_or(cfg!(unix));
    policy.platform.case_sensitive = !config.get_bool("core.ignorecase").unwrap_or(false);
    policy.platform.unicode_normalizing =
        config.get_bool("core.precomposeunicode").unwrap_or(false);

    let repo_config = atomic_config::RepoConfig::load(&repo_root.join(".atomic/config.toml"))
        .map_err(|error| git_error(format!("cannot load Atomic content-filter policy: {error}")))?;
    let mut relevant = Vec::new();
    relevant.extend_from_slice(format!("object={object_format:?}\n").as_bytes());
    relevant.extend_from_slice(
        format!(
            "filemode={}\nsymlinks={}\nignorecase={}\nprecomposeunicode={}\ntimeout={}\nmax-output={}\n",
            policy.platform.executable_bit,
            policy.platform.symlinks,
            !policy.platform.case_sensitive,
            policy.platform.unicode_normalizing,
            repo_config.filters.timeout_ms,
            repo_config.filters.max_output_bytes
        )
        .as_bytes(),
    );
    policy.relevant_git_config = content_key(&relevant);
    policy.filter_driver_versions = repo_config
        .filters
        .drivers
        .iter()
        .map(|(name, driver)| {
            content_key(
                format!(
                    "{name}\0{}\0{}\0{}",
                    driver.clean.as_deref().unwrap_or(""),
                    driver.smudge.as_deref().unwrap_or(""),
                    driver.required
                )
                .as_bytes(),
            )
        })
        .collect();
    Ok((policy, repo_config.filters))
}

/// Result of coordinating an Atomic view switch with its local Git shadow.
#[derive(Debug)]
pub(crate) enum ShadowSwitchSync {
    /// The working copy is not inside a Git repository.
    SkippedNoGit,
    /// Git exists, but Atomic shadow sync has not been established.
    SkippedInactive,
    /// The target branch, HEAD, and index now describe the materialized view.
    Synchronized(ShadowSwitchReceipt),
}

impl ShadowSwitchSync {
    pub(crate) fn is_synchronized(&self) -> bool {
        matches!(self, Self::Synchronized(_))
    }

    /// Restore the Git evidence captured before synchronization. This is used
    /// when the final bridge checkpoint cannot be refreshed after Git itself
    /// was aligned successfully.
    pub(crate) fn rollback(self, repo_root: &Path) -> CliResult<()> {
        let Self::Synchronized(receipt) = self else {
            return Ok(());
        };
        let git_repo = GitRepository::discover(repo_root).map_err(|error| {
            git_error(format!(
                "cannot reopen Git repository to roll back shadow switch: {error}"
            ))
        })?;
        let failures = receipt.snapshot.restore(&git_repo, &receipt.target_ref);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(git_error(format!(
                "failed to restore original Git state: {}",
                failures.join("; ")
            )))
        }
    }
}

#[derive(Debug)]
pub(crate) struct ShadowSwitchReceipt {
    target_ref: String,
    snapshot: GitSwitchSnapshot,
}

#[derive(Clone, Debug)]
enum ReferenceSnapshot {
    Missing,
    Direct(git2::Oid),
    Symbolic(String),
}

#[derive(Clone, Debug)]
struct GitSwitchSnapshot {
    head: ReferenceSnapshot,
    target: ReferenceSnapshot,
    index: Option<Vec<u8>>,
}

impl GitSwitchSnapshot {
    fn capture(git_repo: &GitRepository, target_ref: &str) -> CliResult<Self> {
        let index_path = git_repo.path().join("index");
        let index = match std::fs::read(&index_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(git_error(format!(
                    "cannot snapshot Git index before shadow switch: {error}"
                )))
            }
        };
        Ok(Self {
            head: snapshot_reference(git_repo, "HEAD")?,
            target: snapshot_reference(git_repo, target_ref)?,
            index,
        })
    }

    fn restore(&self, git_repo: &GitRepository, target_ref: &str) -> Vec<String> {
        let mut failures = Vec::new();
        if let Err(error) = restore_head(git_repo, &self.head) {
            failures.push(format!("HEAD: {error}"));
        }
        if let Err(error) = restore_reference(git_repo, target_ref, &self.target) {
            failures.push(format!("{target_ref}: {error}"));
        }
        if let Err(error) = restore_index(git_repo, self.index.as_deref()) {
            failures.push(format!("index: {error}"));
        }
        failures
    }
}

/// Project the just-materialized Atomic view into its local Git mirror.
///
/// The working copy is staged exclusively through [`stage_and_validate_tree`].
/// If the target branch is stale, a local projection commit advances it from
/// its prior tip; a missing target branches from the current Git HEAD. HEAD and
/// the live index are then aligned to that exact tree without checking out or
/// otherwise touching the working copy.
///
/// Plain non-Git and non-shadow repositories are typed no-ops. Once shadow sync
/// is active, every failure is returned to the caller and the original Git
/// HEAD, target ref, and index are restored where possible.
pub(crate) fn sync_git_head_to_view(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<ShadowSwitchSync> {
    let git_repo = match GitRepository::discover(repo_root) {
        Ok(repo) => repo,
        Err(_) => return Ok(ShadowSwitchSync::SkippedNoGit),
    };
    if !shadow_sync_active(&git_repo) {
        return Ok(ShadowSwitchSync::SkippedInactive);
    }
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;
    let desired_view = repo
        .desired_view_name(working_copy)
        .map_err(CliError::Repository)?;
    if desired_view != view {
        return Err(git_error(format!(
            "cannot synchronize Git shadow for view '{view}': current Atomic view is '{desired_view}'"
        )));
    }

    // A coordinated switch must not silently skip its projection. Contention is
    // an actionable failure because leaving the old branch/index would publish
    // stale evidence for the newly materialized view.
    let _shadow_lock = repo
        .try_lock_shadow_commit()
        .map_err(CliError::Repository)?
        .ok_or_else(|| {
            git_error(
                "another shadow materialize is in flight; retry the view switch after it completes",
            )
        })?;

    let target_ref = format!("refs/heads/{view}");
    let snapshot = GitSwitchSnapshot::capture(&git_repo, &target_ref)?;
    let sync_result = (|| -> CliResult<()> {
        let publication = verify_git_publication(repo, repo_root, view)?;
        // Never bypass V1 for a switch: a conflicted materialization cannot be
        // checkpointed as clean Git evidence.
        let tree_oid = stage_and_validate_tree(
            repo,
            &git_repo,
            repo_root,
            &publication,
            ConflictMarkerPolicy::Refuse,
        )?;
        publication.reobserve(repo, repo_root)?;
        let commit = find_or_create_switch_projection(
            &git_repo,
            &target_ref,
            view,
            publication.atomic_state(),
            tree_oid,
            &publication,
        )?;
        publication.reobserve(repo, repo_root)?;
        update_target_ref(&git_repo, &target_ref, commit, &publication)?;
        publication.reobserve(repo, repo_root)?;
        git_repo.set_head(&target_ref).map_err(|error| {
            git_error(format!(
                "cannot point Git HEAD at shadow branch '{view}': {error}"
            ))
        })?;
        publication.reobserve(repo, repo_root)?;
        align_index_to_tree(&git_repo, tree_oid, &publication)?;
        Ok(())
    })();

    match sync_result {
        Ok(()) => Ok(ShadowSwitchSync::Synchronized(ShadowSwitchReceipt {
            target_ref,
            snapshot,
        })),
        Err(error) => {
            let rollback_failures = snapshot.restore(&git_repo, &target_ref);
            if rollback_failures.is_empty() {
                Err(error)
            } else {
                Err(git_error(format!(
                    "{error}; additionally failed to restore original Git state: {}",
                    rollback_failures.join("; ")
                )))
            }
        }
    }
}

fn find_or_create_switch_projection(
    git_repo: &GitRepository,
    target_ref: &str,
    view: &str,
    state: &str,
    tree_oid: git2::Oid,
    publication: &VerifiedPublication,
) -> CliResult<git2::Oid> {
    if publication.view() != view || publication.git_tree_oid()? != tree_oid {
        return Err(git_error(
            "shadow projection does not match verified publication",
        ));
    }
    let parent_oid = match git_repo.find_reference(target_ref) {
        Ok(reference) => {
            let commit = reference.peel_to_commit().map_err(|error| {
                git_error(format!("cannot read target shadow branch '{view}': {error}"))
            })?;
            if commit.tree_id() == tree_oid {
                return Ok(commit.id());
            }
            commit.id()
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => git_repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(|error| {
                git_error(format!(
                    "cannot create shadow branch '{view}' without a current Git HEAD commit: {error}"
                ))
            })?
            .id(),
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target shadow branch '{view}': {error}"
            )))
        }
    };
    let parent = git_repo.find_commit(parent_oid).map_err(|error| {
        git_error(format!(
            "cannot read parent for shadow projection on '{view}': {error}"
        ))
    })?;
    let tree = git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "cannot read validated tree for shadow projection on '{view}': {error}"
        ))
    })?;
    let signature = git_repo.signature().map_err(|error| {
        git_error(format!(
            "cannot determine Git signature for shadow projection: {error}"
        ))
    })?;
    let message =
        format!("Atomic shadow switch projection\n\nAtomic-View: {view}\nAtomic-State: {state}\n");
    let commit = git_repo
        .commit(None, &signature, &signature, &message, &tree, &[&parent])
        .map_err(|error| {
            git_error(format!(
                "cannot create local shadow projection for view '{view}': {error}"
            ))
        })?;
    Ok(commit)
}

fn update_target_ref(
    git_repo: &GitRepository,
    target_ref: &str,
    commit: git2::Oid,
    publication: &VerifiedPublication,
) -> CliResult<()> {
    let _verified_tree = publication.git_tree_oid()?;
    match git_repo.find_reference(target_ref) {
        Ok(mut reference) => {
            if reference.target() != Some(commit) {
                reference
                    .set_target(commit, "atomic shadow view switch")
                    .map_err(|error| {
                        git_error(format!(
                            "cannot advance target shadow branch '{target_ref}': {error}"
                        ))
                    })?;
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            git_repo
                .reference(target_ref, commit, false, "atomic shadow view switch")
                .map_err(|error| {
                    git_error(format!(
                        "cannot create target shadow branch '{target_ref}': {error}"
                    ))
                })?;
        }
        Err(error) => {
            return Err(git_error(format!(
                "cannot inspect target shadow branch '{target_ref}': {error}"
            )))
        }
    }
    Ok(())
}

fn align_index_to_tree(
    git_repo: &GitRepository,
    tree_oid: git2::Oid,
    publication: &VerifiedPublication,
) -> CliResult<()> {
    if publication.git_tree_oid()? != tree_oid {
        return Err(git_error("refusing to align index to an unverified tree"));
    }
    let tree = git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "cannot read shadow projection tree while aligning Git index: {error}"
        ))
    })?;
    let mut index = git_repo.index().map_err(|error| {
        git_error(format!(
            "cannot open Git index while aligning shadow view: {error}"
        ))
    })?;
    index
        .read_tree(&tree)
        .and_then(|_| index.write())
        .map_err(|error| {
            git_error(format!(
                "cannot align Git index to the shadow projection tree: {error}"
            ))
        })
}

fn snapshot_reference(git_repo: &GitRepository, name: &str) -> CliResult<ReferenceSnapshot> {
    match git_repo.find_reference(name) {
        Ok(reference) => {
            if let Some(target) = reference.target() {
                Ok(ReferenceSnapshot::Direct(target))
            } else if let Some(target) = reference.symbolic_target() {
                Ok(ReferenceSnapshot::Symbolic(target.to_string()))
            } else {
                Err(git_error(format!(
                    "cannot snapshot Git reference '{name}': it has no target"
                )))
            }
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(ReferenceSnapshot::Missing),
        Err(error) => Err(git_error(format!(
            "cannot snapshot Git reference '{name}': {error}"
        ))),
    }
}

fn restore_head(git_repo: &GitRepository, snapshot: &ReferenceSnapshot) -> Result<(), git2::Error> {
    match snapshot {
        ReferenceSnapshot::Missing => match git_repo.find_reference("HEAD") {
            Ok(mut reference) => reference.delete(),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        ReferenceSnapshot::Direct(target) => git_repo.set_head_detached(*target),
        ReferenceSnapshot::Symbolic(target) => git_repo.set_head(target),
    }
}

fn restore_reference(
    git_repo: &GitRepository,
    name: &str,
    snapshot: &ReferenceSnapshot,
) -> Result<(), git2::Error> {
    match snapshot {
        ReferenceSnapshot::Missing => match git_repo.find_reference(name) {
            Ok(mut reference) => reference.delete(),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        ReferenceSnapshot::Direct(target) => git_repo
            .reference(name, *target, true, "restore failed Atomic shadow switch")
            .map(|_| ()),
        ReferenceSnapshot::Symbolic(target) => git_repo
            .reference_symbolic(name, target, true, "restore failed Atomic shadow switch")
            .map(|_| ()),
    }
}

fn restore_index(git_repo: &GitRepository, bytes: Option<&[u8]>) -> std::io::Result<()> {
    let index_path = git_repo.path().join("index");
    match bytes {
        Some(bytes) => std::fs::write(index_path, bytes),
        None => match std::fs::remove_file(index_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

fn git_error(message: impl Into<String>) -> CliError {
    CliError::GitError {
        message: message.into(),
    }
}

/// Whether git shadow sync is established for this repo (the `.git/info/exclude`
/// carries Atomic's shadow patterns, written by import/push). Used to gate the
/// view-switch git-follow so we never touch HEAD in a plain (non-shadow) repo.
fn shadow_sync_active(git_repo: &GitRepository) -> bool {
    let exclude = git_repo.path().join("info").join("exclude");
    std::fs::read_to_string(exclude)
        .map(|c| c.lines().any(|l| l.trim() == "/.atomic/"))
        .unwrap_or(false)
}

/// Acquire the repo-scoped shadow-commit lock, or return `None` (a no-op skip)
/// if a shadow materialize/commit is already in flight (SPEC §4.3 / Principle 5).
///
/// Non-blocking: rather than queueing (which would hang a turn-end hook), the
/// contended case is a logged no-op — the in-flight operation owns this commit.
/// The returned guard must be held for the whole stage → validate → commit
/// sequence; dropping it releases the lock. Acquire it **outermost**, before any
/// staging or DB write.
pub(crate) fn acquire_shadow_lock(
    repo: &Repository,
    repo_root: &Path,
    view: &str,
) -> CliResult<Option<atomic_repository::RepositoryCommonLockGuard>> {
    match repo
        .try_lock_shadow_commit()
        .map_err(CliError::Repository)?
    {
        Some(guard) => Ok(Some(guard)),
        None => {
            if std::io::stderr().is_terminal() {
                print_info("Another shadow materialize is in flight; skipping this push.");
            } else {
                append_shadow_log(
                    repo_root,
                    "shadow-lock:contended",
                    view,
                    "another shadow materialize in flight",
                );
            }
            Ok(None)
        }
    }
}

/// Validate the already-observed index for a shadow commit and return its tree.
///
/// CB-4B requires the index and worktree to be equivalent before this function
/// can be called. This path therefore does not stage or write an ODB tree; it
/// only enforces the marker/provenance rules and resolves the verified tree.
pub(crate) fn stage_and_validate_tree(
    repo: &Repository,
    git_repo: &GitRepository,
    repo_root: &Path,
    publication: &VerifiedPublication,
    conflict_markers: ConflictMarkerPolicy,
) -> CliResult<git2::Oid> {
    let view = publication.view();
    let working_copy = repo
        .require_working_copy_id()
        .map_err(CliError::Repository)?;

    // ── Rule V1 — no unresolved conflict markers ────────────────────────────
    // Shares `atomic record`'s detector so the two paths cannot disagree.
    if conflict_markers == ConflictMarkerPolicy::Refuse {
        if let Some((path, line)) = repo
            .first_working_copy_conflict_marker(working_copy)
            .map_err(CliError::Repository)?
        {
            if !std::io::stderr().is_terminal() {
                append_shadow_validate_log(
                    repo_root,
                    "V1",
                    view,
                    &format!("file={} line={}", path, line),
                );
            }
            print_warning(&format!(
                "Refusing to commit '{}': unresolved conflict marker at line {}.",
                path, line
            ));
            return Err(CliError::GitError {
                message: format!(
                    "'{}' still contains conflict markers at line {} — resolve the \
                     conflict (remove the >>>>>>> / ======= / <<<<<<< lines), or pass \
                     --allow-conflict-markers to override. No commit was created.",
                    path, line
                ),
            });
        }
    }

    // Prevention: make sure git is configured to exclude Atomic's shadow /
    // provenance paths before staging, so `git add -A` never picks them up.
    // Best-effort (an unwritable .git/info is caught by the V4 guard below).
    let _ = super::import::ensure_git_shadow_excludes(git_repo.path());

    // CB-4B already proved that the read-only index exactly represents the
    // Atomic project tree. Do not restage or write an ODB tree here: doing so
    // would turn validation itself into a mutation and reopen a TOCTOU window.
    let index = git_repo.index().map_err(|e| CliError::GitError {
        message: format!("Failed to open git index: {}", e),
    })?;

    // ── Rule V4 — no provenance / excluded path may be staged ───────────────
    if let Some(bad) = first_forbidden_shadow_path(&index) {
        if !std::io::stderr().is_terminal() {
            append_shadow_validate_log(repo_root, "V4", view, &format!("path={}", bad));
        }
        print_warning(&format!(
            "Refusing to shadow-commit: provenance/excluded path '{}' was staged.",
            bad
        ));
        return Err(CliError::GitError {
            message: format!(
                "'{}' is a git-excluded Atomic shadow path (.atomic/, .vault/, \
                 .atomicignore) and must never be committed to git. Aborting; no \
                 commit was created and the index was restored. Ensure \
                 `.git/info/exclude` carries the Atomic shadow patterns.",
                bad
            ),
        });
    }

    let tree_oid = publication.git_tree_oid()?;
    git_repo.find_tree(tree_oid).map_err(|error| {
        git_error(format!(
            "verified index tree {tree_oid} is absent from the Git object database: {error}"
        ))
    })?;
    Ok(tree_oid)
}

/// Append a `shadow-validate:<rule>` entry to `.atomic/hook-errors.log` (SPEC
/// §6.5) so a non-interactive shadow push that a Validator rule aborts leaves a
/// durable, greppable trail instead of failing silently.
pub(crate) fn append_shadow_validate_log(repo_root: &Path, rule: &str, view: &str, detail: &str) {
    append_shadow_log(
        repo_root,
        &format!("shadow-validate:{}", rule),
        view,
        detail,
    );
}

/// Append one tagged `.atomic/hook-errors.log` line. Best-effort: log I/O errors
/// are ignored (the operation already surfaces its own outcome).
fn append_shadow_log(repo_root: &Path, tag: &str, view: &str, detail: &str) {
    use std::io::Write;
    let log_path = repo_root.join(".atomic").join("hook-errors.log");
    let entry = format!(
        "{} {} view={} {}\n",
        chrono::Utc::now().to_rfc3339(),
        tag,
        view,
        detail
    );
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .and_then(|mut f| f.write_all(entry.as_bytes()));
}

/// Return the first staged index path that is a git-excluded shadow / provenance
/// path (`.atomic/`, `.vault/`, or `.atomicignore`), or `None` if the candidate
/// is clean. Validator Rule V4 (SPEC §6.4): these paths must never enter a git
/// commit — `.vault` (intents/memories/attestations) and `.atomic` (the change
/// graph) are git-excluded and unbacked; committing or reconciling them risks
/// the provenance layer.
fn first_forbidden_shadow_path(index: &git2::Index) -> Option<String> {
    index.iter().find_map(|entry| {
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        is_forbidden_shadow_path(&path).then_some(path)
    })
}

/// Whether `path` (a repo-relative git path) is a git-excluded Atomic shadow /
/// provenance path that Rule V4 forbids from any shadow commit.
fn is_forbidden_shadow_path(path: &str) -> bool {
    path == ".atomicignore" || path.starts_with(".atomic/") || path.starts_with(".vault/")
}

#[cfg(test)]
mod tests {
    use super::is_forbidden_shadow_path as forbidden;

    #[test]
    fn forbids_provenance_and_excluded_paths() {
        assert!(forbidden(".atomicignore"));
        assert!(forbidden(".atomic/pristine.redb"));
        assert!(forbidden(".vault/intents/foo.md"));
    }

    #[test]
    fn allows_ordinary_source_paths() {
        assert!(!forbidden("src/main.rs"));
        assert!(!forbidden("README.md"));
        // A file that merely *contains* the substring is not forbidden.
        assert!(!forbidden("docs/.atomicignore.md"));
        assert!(!forbidden("my.vault/keep.txt"));
    }
}

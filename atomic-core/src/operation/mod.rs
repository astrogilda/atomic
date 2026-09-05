//! Immutable repository operations, external-effect plans, and receipts.
//!
//! Operations hash their complete immutable canonical payload. The derived ID,
//! mutable operation heads, effect receipts, and execution state are deliberately
//! stored outside that payload, preventing self-reference and in-place phase
//! mutation.

mod codec;

use std::fmt;

use crate::types::{EffectReceiptId, Hash, Merkle, OperationId, SetId, WorkingCopyId};

pub use codec::{
    decode_effect_receipt, decode_operation, decode_operation_heads, decode_operation_scope,
    encode_effect_receipt, encode_operation, encode_operation_heads, encode_operation_scope,
    EFFECT_RECEIPT_VERSION, OPERATION_HEADS_VERSION, OPERATION_VERSION,
};

/// Maximum canonical object size accepted by the operation codec.
pub const MAX_OPERATION_OBJECT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of entries accepted in any canonical operation collection.
pub const MAX_OPERATION_COLLECTION_ITEMS: usize = 16_384;
/// Maximum UTF-8 byte length accepted for one canonical string.
pub const MAX_OPERATION_STRING_BYTES: usize = 1024 * 1024;

/// Error returned for malformed, unsupported, or noncanonical operation data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationCodecError {
    message: String,
}

impl OperationCodecError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Diagnostic describing the rejected canonical data.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for OperationCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OperationCodecError {}

/// Immutable operation kinds reserved by the causal-bridge RFC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperationKind {
    Anchor,
    Record,
    PromoteSnapshot,
    SplitSnapshot,
    SwitchView,
    Materialize,
    ImportGitHead,
    ImportGitRefs,
    ExportGitRefs,
    ProjectState,
    SynthesizeGit,
    ResurrectBinding,
    Insert,
    Unrecord,
    Tag,
    Recover,
    Undo,
    Gc,
}

/// Scope whose mutable head set points at immutable operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperationScope {
    /// Repository-wide operations without a physical working copy.
    Repository,
    /// Operations belonging to one persistent working copy.
    WorkingCopy(WorkingCopyId),
}

impl fmt::Display for OperationScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Repository => formatter.write_str("repository"),
            Self::WorkingCopy(id) => write!(formatter, "working-copy:{id}"),
        }
    }
}

/// Canonical sorted set of current operation heads for one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationHeads {
    heads: Vec<OperationId>,
}

impl OperationHeads {
    /// Construct a canonical sorted, deduplicated head set.
    pub fn new(mut heads: Vec<OperationId>) -> Self {
        heads.sort_unstable();
        heads.dedup();
        Self { heads }
    }

    pub(crate) fn from_canonical(heads: Vec<OperationId>) -> Result<Self, OperationCodecError> {
        ensure_strictly_sorted(&heads, "operation heads")?;
        Ok(Self { heads })
    }

    /// Borrow heads in canonical ascending order.
    pub fn as_slice(&self) -> &[OperationId] {
        &self.heads
    }

    /// Consume this set and return canonical ascending IDs.
    pub fn into_vec(self) -> Vec<OperationId> {
        self.heads
    }

    /// Whether the head set is empty.
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }
}

/// Complete immutable operation payload. The derived [`OperationId`] is not a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPayload {
    pub parents: Vec<OperationId>,
    pub kind: OperationKind,
    pub working_copy: Option<WorkingCopyId>,
    pub before: RepoStateRef,
    pub delta: RepoStateDelta,
    pub git_observed: Vec<GitRefObservation>,
    pub evidence: Vec<Hash>,
    pub actor: ActorRef,
    pub timestamp_ms: i64,
    pub lossy: Vec<OperationLossNote>,
}

impl OperationPayload {
    fn canonicalize(&mut self) -> Result<(), OperationCodecError> {
        self.parents.sort_unstable();
        self.parents.dedup();
        self.evidence.sort_unstable();
        self.evidence.dedup();
        self.git_observed
            .sort_by(|left, right| left.name.cmp(&right.name));
        reject_duplicate_git_ref_names(&self.git_observed)?;
        for note in &mut self.lossy {
            note.evidence.sort_unstable();
            note.evidence.dedup();
        }
        self.lossy.sort_unstable();
        self.lossy.dedup();
        self.delta.canonicalize()?;
        self.validate_canonical()
    }

    pub(crate) fn validate_canonical(&self) -> Result<(), OperationCodecError> {
        ensure_strictly_sorted(&self.parents, "operation parents")?;
        ensure_strictly_sorted(&self.evidence, "operation evidence")?;
        ensure_strictly_sorted(&self.lossy, "operation loss notes")?;
        ensure_git_ref_observations_sorted(&self.git_observed)?;
        for note in &self.lossy {
            ensure_strictly_sorted(&note.evidence, "loss-note evidence")?;
        }
        for state in [&self.before, &self.delta.after] {
            if let Some(state_working_copy) = &state.working_copy {
                match self.working_copy {
                    Some(operation_working_copy)
                        if operation_working_copy == state_working_copy.id => {}
                    Some(operation_working_copy) => {
                        return Err(OperationCodecError::new(format!(
                            "operation working copy {operation_working_copy} disagrees with state working copy {}",
                            state_working_copy.id
                        )));
                    }
                    None => {
                        return Err(OperationCodecError::new(
                            "repository-scoped operation cannot carry working-copy state",
                        ));
                    }
                }
            }
        }
        self.delta.validate_canonical()?;
        match self.kind {
            OperationKind::Anchor if !self.parents.is_empty() => Err(OperationCodecError::new(
                "anchor operations cannot have parents",
            )),
            OperationKind::Anchor => Ok(()),
            _ if self.parents.is_empty() => Err(OperationCodecError::new(
                "non-anchor operations require at least one parent",
            )),
            _ => Ok(()),
        }
    }
}

/// Content-addressed immutable repository operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    id: OperationId,
    payload: OperationPayload,
}

impl Operation {
    /// Canonicalize an immutable payload and derive its content address.
    pub fn new(mut payload: OperationPayload) -> Result<Self, OperationCodecError> {
        payload.canonicalize()?;
        let bytes = codec::encode_operation_payload(&payload)?;
        let id = OperationId::from_canonical_bytes(&bytes);
        if payload.parents.binary_search(&id).is_ok() {
            return Err(OperationCodecError::new(
                "an operation cannot name itself as a parent",
            ));
        }
        Ok(Self { id, payload })
    }

    pub(crate) fn from_canonical_payload(
        id: OperationId,
        payload: OperationPayload,
    ) -> Result<Self, OperationCodecError> {
        payload.validate_canonical()?;
        if payload.parents.binary_search(&id).is_ok() {
            return Err(OperationCodecError::new(
                "an operation cannot name itself as a parent",
            ));
        }
        Ok(Self { id, payload })
    }

    /// Content address of the immutable payload.
    pub fn id(&self) -> OperationId {
        self.id
    }

    /// Borrow the complete immutable payload.
    pub fn payload(&self) -> &OperationPayload {
        &self.payload
    }

    /// Encode the payload using the strict canonical format.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, OperationCodecError> {
        encode_operation(self)
    }
}

/// Repository state observed before or selected after an operation.
///
/// It deliberately has no operation-head or last-operation field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStateRef {
    pub view: Option<ViewStateRef>,
    pub working_copy: Option<WorkingCopyStateRef>,
    pub git: Option<GitStateRef>,
}

impl RepoStateRef {
    /// State with no selected view, working copy, or Git backend.
    pub const EMPTY: Self = Self {
        view: None,
        working_copy: None,
        git: None,
    };
}

/// Immutable intended transition and its ordered external-effect plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStateDelta {
    pub after: RepoStateRef,
    pub effects: Vec<EffectPlan>,
}

impl RepoStateDelta {
    fn canonicalize(&mut self) -> Result<(), OperationCodecError> {
        self.effects.sort_by_key(|effect| effect.ordinal);
        self.validate_canonical()
    }

    fn validate_canonical(&self) -> Result<(), OperationCodecError> {
        for (expected, effect) in self.effects.iter().enumerate() {
            let expected = u32::try_from(expected)
                .map_err(|_| OperationCodecError::new("too many operation effects"))?;
            if effect.ordinal != expected {
                return Err(OperationCodecError::new(format!(
                    "effect ordinals must be contiguous from zero; expected {expected}, found {}",
                    effect.ordinal
                )));
            }
            if effect.expected_old == effect.expected_new {
                return Err(OperationCodecError::new(format!(
                    "effect {} has identical expected-old and expected-new values",
                    effect.ordinal
                )));
            }
        }
        Ok(())
    }
}

/// Stable view identity carried by an operation without repository-local pointers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewStateRef {
    pub name: String,
    pub state: Merkle,
    pub set_id: Option<SetId>,
}

/// Working-copy state without an operation/head pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingCopyStateRef {
    pub id: WorkingCopyId,
    pub location_fingerprint: Hash,
    pub desired_view: u64,
    pub desired_state: Merkle,
    pub materialized_state: Option<Merkle>,
    pub materialized_manifest: Option<Hash>,
}

/// Git state expressed without `git2` or platform-native path types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStateRef {
    pub head: GitHeadState,
    pub index: Option<GitIndexState>,
    pub refs_digest: Hash,
}

/// Git HEAD states that must remain distinguishable during recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHeadState {
    Attached { symref: String, oid: GitObjectId },
    Detached { oid: GitObjectId },
    Unborn { symref: String },
    MissingTarget { symref: String },
}

/// Hash algorithm carried with a Git object identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitHashAlgorithm {
    Sha1,
    Sha256,
}

/// Canonical Git object identity independent of `git2::Oid`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitObjectId {
    algorithm: GitHashAlgorithm,
    bytes: Vec<u8>,
}

impl GitObjectId {
    /// Construct and validate an algorithm-tagged object identity.
    pub fn new(algorithm: GitHashAlgorithm, bytes: Vec<u8>) -> Result<Self, OperationCodecError> {
        let expected = match algorithm {
            GitHashAlgorithm::Sha1 => 20,
            GitHashAlgorithm::Sha256 => 32,
        };
        if bytes.len() != expected {
            return Err(OperationCodecError::new(format!(
                "{:?} Git object ID has length {}, expected {expected}",
                algorithm,
                bytes.len()
            )));
        }
        Ok(Self { algorithm, bytes })
    }

    /// Hash algorithm used by this object identity.
    pub fn algorithm(&self) -> GitHashAlgorithm {
        self.algorithm
    }

    /// Raw digest bytes, whose width is determined by [`Self::algorithm`].
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for GitObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitObjectId")
            .field("algorithm", &self.algorithm)
            .field("bytes", &hex_bytes(&self.bytes))
            .finish()
    }
}

/// Canonical semantic Git index observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitIndexState {
    pub digest: Hash,
    pub tree: Option<GitObjectId>,
}

/// Direct or symbolic Git reference target.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GitRefTarget {
    Direct(GitObjectId),
    Symbolic(String),
}

/// One immutable Git-reference observation, sorted by ref name in an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRefObservation {
    pub name: String,
    pub target: Option<GitRefTarget>,
}

/// Actor responsible for preparing an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorRef {
    Human {
        did: String,
    },
    Agent {
        did: String,
        session: String,
        turn: Option<u64>,
    },
    System {
        name: String,
    },
}

/// Durable structured loss evidence associated with an operation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationLossNote {
    pub code: String,
    pub message: String,
    pub evidence: Vec<Hash>,
}

/// One ordered external effect with expected-old/new leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectPlan {
    pub ordinal: u32,
    pub target: EffectTarget,
    pub expected_old: EffectValue,
    pub expected_new: EffectValue,
}

/// Backend-neutral external-effect target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectTarget {
    FilesystemPath {
        path: String,
    },
    /// Ignored/private entry in the physical working copy, observed recursively
    /// when it is a directory because shelving moves the complete subtree.
    WorkspacePath {
        working_copy: WorkingCopyId,
        path: String,
    },
    ShelfPath {
        working_copy: WorkingCopyId,
        view: String,
        path: String,
    },
    GitObject {
        object: GitObjectId,
    },
    GitIndex {
        working_copy: WorkingCopyId,
    },
    GitRef {
        name: String,
    },
    GitHead {
        working_copy: WorkingCopyId,
    },
    Checkpoint {
        working_copy: WorkingCopyId,
        kind: CheckpointKind,
    },
    WorkingCopy {
        working_copy: WorkingCopyId,
    },
    Verification {
        working_copy: Option<WorkingCopyId>,
        scope: VerificationScope,
    },
}

/// Durable checkpoint families addressed by an effect plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointKind {
    Bridge,
    WorkingCopyCompatibility,
    Recovery,
}

/// Scope covered by a verification effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationScope {
    Filesystem,
    Git,
    Repository,
    Complete,
}

/// Typed expected or observed value used by effect leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectValue {
    Absent,
    Digest { kind: DigestKind, hash: Hash },
    File(FileState),
    GitObject(GitObjectId),
    GitRef(GitRefTarget),
    GitIndex(GitIndexState),
    WorkingCopy(WorkingCopyStateRef),
    Verification(Hash),
}

/// Semantic domain of a generic digest lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestKind {
    Bytes,
    Manifest,
    Checkpoint,
    Shelf,
    Refs,
}

/// Canonical filesystem state without platform-native paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    pub kind: FileKind,
    pub mode: u32,
    pub content: Hash,
}

/// Filesystem entry kinds relevant to materialization and shelving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Directory,
    Symlink,
    Gitlink,
}

/// Immutable outcome represented by an effect receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectReceiptKind {
    Applied,
    Verified,
    RolledBack,
    LeaseRejected,
    Recovered,
}

/// Complete immutable receipt payload. The derived [`EffectReceiptId`] is not a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectReceiptPayload {
    pub operation: OperationId,
    /// `None` is reserved for the final operation-level verified receipt.
    pub effect_ordinal: Option<u32>,
    pub attempt: u32,
    pub kind: EffectReceiptKind,
    pub observed_old: Option<EffectValue>,
    pub observed_new: Option<EffectValue>,
    pub timestamp_ms: i64,
}

impl EffectReceiptPayload {
    fn validate(&self) -> Result<(), OperationCodecError> {
        match (self.kind, self.effect_ordinal) {
            (EffectReceiptKind::Verified, None) => Ok(()),
            (EffectReceiptKind::Verified, Some(_)) => Err(OperationCodecError::new(
                "operation-level verified receipts cannot name an effect ordinal",
            )),
            (_, None) => Err(OperationCodecError::new(
                "non-verified receipts require an effect ordinal",
            )),
            (_, Some(_)) => Ok(()),
        }
    }
}

/// Content-addressed immutable external-effect receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectReceipt {
    id: EffectReceiptId,
    payload: EffectReceiptPayload,
}

impl EffectReceipt {
    /// Validate an immutable payload and derive its content address.
    pub fn new(payload: EffectReceiptPayload) -> Result<Self, OperationCodecError> {
        payload.validate()?;
        let bytes = codec::encode_effect_receipt_payload(&payload)?;
        let id = EffectReceiptId::from_canonical_bytes(&bytes);
        Ok(Self { id, payload })
    }

    pub(crate) fn from_canonical_payload(
        id: EffectReceiptId,
        payload: EffectReceiptPayload,
    ) -> Result<Self, OperationCodecError> {
        payload.validate()?;
        Ok(Self { id, payload })
    }

    /// Content address of the immutable receipt payload.
    pub fn id(&self) -> EffectReceiptId {
        self.id
    }

    /// Borrow the complete immutable receipt payload.
    pub fn payload(&self) -> &EffectReceiptPayload {
        &self.payload
    }

    /// Encode the payload using the strict canonical format.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, OperationCodecError> {
        encode_effect_receipt(self)
    }
}

fn ensure_strictly_sorted<T: Ord>(values: &[T], field: &str) -> Result<(), OperationCodecError> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(OperationCodecError::new(format!(
            "{field} must be strictly sorted and unique"
        )));
    }
    Ok(())
}

fn ensure_git_ref_observations_sorted(
    values: &[GitRefObservation],
) -> Result<(), OperationCodecError> {
    if values.windows(2).any(|pair| pair[0].name >= pair[1].name) {
        return Err(OperationCodecError::new(
            "Git ref observations must be strictly sorted by name and unique",
        ));
    }
    Ok(())
}

fn reject_duplicate_git_ref_names(values: &[GitRefObservation]) -> Result<(), OperationCodecError> {
    if values.windows(2).any(|pair| pair[0].name == pair[1].name) {
        return Err(OperationCodecError::new(
            "Git ref observations contain duplicate names",
        ));
    }
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

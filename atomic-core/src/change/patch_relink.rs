//! Signed, content-addressed mapping from graph positions in one change to another.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

use crate::types::{GraphNode, Hash};

const MAGIC: &[u8; 4] = b"PTRL";
const SCHEMA_VERSION: u8 = 1;
const SIGNING_DOMAIN: &[u8] = b"atomic:patch-relink:v1\0";
const SIGNATURE_LENGTH: usize = 64;

/// Maps a node in the replaced change to its corresponding node in the new change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionRelink {
    old: GraphNode<Hash>,
    new: GraphNode<Hash>,
}

impl PositionRelink {
    pub fn new(old: GraphNode<Hash>, new: GraphNode<Hash>) -> Self {
        Self { old, new }
    }

    pub fn old(&self) -> &GraphNode<Hash> {
        &self.old
    }

    pub fn new_node(&self) -> &GraphNode<Hash> {
        &self.new
    }
}

/// An immutable statement mapping graph positions from an old change to a new change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchRelink {
    version: u8,
    old_change: Hash,
    new_change: Hash,
    position_map: Vec<PositionRelink>,
    removed_ranges: Vec<GraphNode<Hash>>,
    provenance_relinks: Vec<Hash>,
    signer: Vec<u8>,
    signed_at: i64,
    signature: Vec<u8>,
}

#[derive(Serialize)]
struct UnsignedPatchRelink<'a> {
    version: u8,
    old_change: &'a Hash,
    new_change: &'a Hash,
    position_map: &'a [PositionRelink],
    removed_ranges: &'a [GraphNode<Hash>],
    provenance_relinks: &'a [Hash],
    signer: &'a [u8],
    signed_at: i64,
}

impl PatchRelink {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        old_change: Hash,
        new_change: Hash,
        position_map: Vec<PositionRelink>,
        removed_ranges: Vec<GraphNode<Hash>>,
        provenance_relinks: Vec<Hash>,
        signer: Vec<u8>,
        signed_at: i64,
        signature: Vec<u8>,
    ) -> Result<Self, PatchRelinkError> {
        let relink = Self {
            version: SCHEMA_VERSION,
            old_change,
            new_change,
            position_map,
            removed_ranges,
            provenance_relinks,
            signer,
            signed_at,
            signature,
        };
        relink.validate()?;
        Ok(relink)
    }

    pub fn version(&self) -> u8 {
        self.version
    }
    pub fn old_change(&self) -> &Hash {
        &self.old_change
    }
    pub fn new_change(&self) -> &Hash {
        &self.new_change
    }
    pub fn position_map(&self) -> &[PositionRelink] {
        &self.position_map
    }
    pub fn removed_ranges(&self) -> &[GraphNode<Hash>] {
        &self.removed_ranges
    }
    pub fn provenance_relinks(&self) -> &[Hash] {
        &self.provenance_relinks
    }
    pub fn signer(&self) -> &[u8] {
        &self.signer
    }
    pub fn signed_at(&self) -> i64 {
        self.signed_at
    }
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }

    /// Returns deterministic, domain-separated bytes covering every unsigned field.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, PatchRelinkError> {
        let unsigned = UnsignedPatchRelink {
            version: self.version,
            old_change: &self.old_change,
            new_change: &self.new_change,
            position_map: &self.position_map,
            removed_ranges: &self.removed_ranges,
            provenance_relinks: &self.provenance_relinks,
            signer: &self.signer,
            signed_at: self.signed_at,
        };
        let payload = postcard::to_allocvec(&unsigned).map_err(PatchRelinkError::codec)?;
        let mut bytes = Vec::with_capacity(SIGNING_DOMAIN.len() + payload.len());
        bytes.extend_from_slice(SIGNING_DOMAIN);
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// Serializes this relink and returns the hash of the complete encoding.
    pub fn serialize(&self) -> Result<(Vec<u8>, Hash), PatchRelinkError> {
        self.validate()?;
        let payload = postcard::to_allocvec(self).map_err(PatchRelinkError::codec)?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + payload.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&payload);
        let hash = Hash::of(&bytes);
        Ok((bytes, hash))
    }

    /// Deserializes a relink and returns the hash of the exact input bytes.
    pub fn deserialize(data: &[u8]) -> Result<(Self, Hash), PatchRelinkError> {
        if data.len() < MAGIC.len() + 1 {
            return Err(PatchRelinkError::Codec {
                reason: format!("data too short: {} bytes", data.len()),
            });
        }
        if &data[..MAGIC.len()] != MAGIC {
            return Err(PatchRelinkError::Codec {
                reason: "invalid patch relink magic".into(),
            });
        }
        let relink: Self =
            postcard::from_bytes(&data[MAGIC.len()..]).map_err(PatchRelinkError::codec)?;
        if relink.version != SCHEMA_VERSION {
            return Err(PatchRelinkError::UnsupportedVersion {
                version: relink.version,
                supported: SCHEMA_VERSION,
            });
        }
        relink.validate()?;
        Ok((relink, Hash::of(data)))
    }

    pub fn is_patch_relink(data: &[u8]) -> bool {
        data.starts_with(MAGIC)
    }

    fn validate(&self) -> Result<(), PatchRelinkError> {
        if self.old_change == self.new_change {
            return Err(PatchRelinkError::SameChangeHash);
        }
        if self.signature.len() != SIGNATURE_LENGTH {
            return Err(PatchRelinkError::InvalidSignatureLength {
                actual: self.signature.len(),
            });
        }

        let mut mapped_old = HashSet::with_capacity(self.position_map.len());
        for mapping in &self.position_map {
            if mapping.old.change != self.old_change {
                return Err(PatchRelinkError::OldNodeChangeMismatch);
            }
            if mapping.new.change != self.new_change {
                return Err(PatchRelinkError::NewNodeChangeMismatch);
            }
            if !mapped_old.insert(mapping.old) {
                return Err(PatchRelinkError::DuplicateOldNode);
            }
        }

        for removed in &self.removed_ranges {
            if removed.change != self.old_change {
                return Err(PatchRelinkError::RemovedRangeChangeMismatch);
            }
            if self
                .position_map
                .iter()
                .any(|mapping| ranges_overlap(removed, &mapping.old))
            {
                return Err(PatchRelinkError::RemovedRangeOverlapsMappedNode);
            }
        }
        Ok(())
    }
}

fn ranges_overlap(a: &GraphNode<Hash>, b: &GraphNode<Hash>) -> bool {
    if a.is_empty() || b.is_empty() {
        a.start == b.start
    } else {
        a.start < b.end && b.start < a.end
    }
}

/// Errors produced while constructing or decoding a patch relink.
#[derive(Debug, PartialEq, Eq)]
pub enum PatchRelinkError {
    SameChangeHash,
    InvalidSignatureLength { actual: usize },
    OldNodeChangeMismatch,
    NewNodeChangeMismatch,
    DuplicateOldNode,
    RemovedRangeChangeMismatch,
    RemovedRangeOverlapsMappedNode,
    UnsupportedVersion { version: u8, supported: u8 },
    Codec { reason: String },
}

impl PatchRelinkError {
    fn codec(error: impl fmt::Display) -> Self {
        Self::Codec {
            reason: error.to_string(),
        }
    }
}

impl fmt::Display for PatchRelinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SameChangeHash => write!(f, "old and new change hashes must differ"),
            Self::InvalidSignatureLength { actual } => write!(
                f,
                "signature must be {SIGNATURE_LENGTH} bytes, got {actual}"
            ),
            Self::OldNodeChangeMismatch => {
                write!(f, "mapped old node does not belong to old change")
            }
            Self::NewNodeChangeMismatch => {
                write!(f, "mapped new node does not belong to new change")
            }
            Self::DuplicateOldNode => write!(f, "mapped old nodes must be unique"),
            Self::RemovedRangeChangeMismatch => {
                write!(f, "removed range does not belong to old change")
            }
            Self::RemovedRangeOverlapsMappedNode => {
                write!(f, "removed range overlaps a mapped old node")
            }
            Self::UnsupportedVersion { version, supported } => write!(
                f,
                "unsupported patch relink version {version} (supported: {supported})"
            ),
            Self::Codec { reason } => write!(f, "patch relink codec error: {reason}"),
        }
    }
}

impl std::error::Error for PatchRelinkError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    fn node(change: Hash, start: u64, end: u64) -> GraphNode<Hash> {
        GraphNode::new(change, ChangePosition::new(start), ChangePosition::new(end))
    }

    fn relink_with_signature(signature: Vec<u8>) -> PatchRelink {
        let old = Hash::of(b"old");
        let new = Hash::of(b"new");
        PatchRelink::new(
            old,
            new,
            vec![PositionRelink::new(node(old, 0, 10), node(new, 3, 13))],
            vec![node(old, 10, 20)],
            vec![Hash::of(b"provenance relink")],
            vec![1, 2, 3],
            1_735_689_600,
            signature,
        )
        .unwrap()
    }

    fn relink() -> PatchRelink {
        relink_with_signature(vec![7; SIGNATURE_LENGTH])
    }

    #[test]
    fn roundtrip_is_deterministic_and_content_addressed() {
        let value = relink();
        let (bytes, hash) = value.serialize().unwrap();
        assert!(PatchRelink::is_patch_relink(&bytes));
        assert_eq!(&bytes[..4], MAGIC);
        assert_eq!(hash, Hash::of(&bytes));
        let (decoded, decoded_hash) = PatchRelink::deserialize(&bytes).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(decoded_hash, hash);
        assert_eq!(value.serialize().unwrap(), (bytes, hash));
    }

    #[test]
    fn signing_bytes_are_domain_separated_and_exclude_signature() {
        let first = relink_with_signature(vec![7; SIGNATURE_LENGTH]);
        let second = relink_with_signature(vec![9; SIGNATURE_LENGTH]);
        assert!(first.signing_bytes().unwrap().starts_with(SIGNING_DOMAIN));
        assert_eq!(
            first.signing_bytes().unwrap(),
            second.signing_bytes().unwrap()
        );
        assert_ne!(first.serialize().unwrap().0, second.serialize().unwrap().0);
    }

    #[test]
    fn rejects_equal_hashes_and_bad_signature_length() {
        let hash = Hash::of(b"same");
        assert_eq!(
            PatchRelink::new(hash, hash, vec![], vec![], vec![], vec![], 0, vec![0; 64])
                .unwrap_err(),
            PatchRelinkError::SameChangeHash
        );
        for actual in [0, 63, 65] {
            assert_eq!(
                PatchRelink::new(
                    Hash::of(b"old"),
                    Hash::of(b"new"),
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                    0,
                    vec![0; actual]
                )
                .unwrap_err(),
                PatchRelinkError::InvalidSignatureLength { actual }
            );
        }
    }

    #[test]
    fn validates_position_map_ownership_and_uniqueness() {
        let old = Hash::of(b"old");
        let new = Hash::of(b"new");
        let other = Hash::of(b"other");
        let make = |map| PatchRelink::new(old, new, map, vec![], vec![], vec![], 0, vec![0; 64]);
        assert_eq!(
            make(vec![PositionRelink::new(
                node(other, 0, 1),
                node(new, 0, 1)
            )])
            .unwrap_err(),
            PatchRelinkError::OldNodeChangeMismatch
        );
        assert_eq!(
            make(vec![PositionRelink::new(
                node(old, 0, 1),
                node(other, 0, 1)
            )])
            .unwrap_err(),
            PatchRelinkError::NewNodeChangeMismatch
        );
        let mapping = PositionRelink::new(node(old, 0, 1), node(new, 0, 1));
        assert_eq!(
            make(vec![mapping.clone(), mapping]).unwrap_err(),
            PatchRelinkError::DuplicateOldNode
        );
    }

    #[test]
    fn validates_removed_range_ownership_and_disjointness() {
        let old = Hash::of(b"old");
        let new = Hash::of(b"new");
        let map = vec![PositionRelink::new(node(old, 5, 10), node(new, 0, 5))];
        let make = |removed| {
            PatchRelink::new(
                old,
                new,
                map.clone(),
                removed,
                vec![],
                vec![],
                0,
                vec![0; 64],
            )
        };
        assert_eq!(
            make(vec![node(new, 10, 20)]).unwrap_err(),
            PatchRelinkError::RemovedRangeChangeMismatch
        );
        assert_eq!(
            make(vec![node(old, 9, 12)]).unwrap_err(),
            PatchRelinkError::RemovedRangeOverlapsMappedNode
        );
        assert!(make(vec![node(old, 10, 12)]).is_ok());
    }

    #[test]
    fn rejects_wrong_magic_unsupported_version_and_invalid_payload() {
        let (mut wrong_magic, _) = relink().serialize().unwrap();
        wrong_magic[..4].copy_from_slice(b"NOPE");
        assert!(matches!(
            PatchRelink::deserialize(&wrong_magic),
            Err(PatchRelinkError::Codec { .. })
        ));

        let mut unsupported = relink();
        unsupported.version += 1;
        let mut bytes = MAGIC.to_vec();
        bytes.extend(postcard::to_allocvec(&unsupported).unwrap());
        assert_eq!(
            PatchRelink::deserialize(&bytes).unwrap_err(),
            PatchRelinkError::UnsupportedVersion {
                version: 2,
                supported: 1
            }
        );

        let mut invalid = relink();
        invalid.signature.pop();
        let mut bytes = MAGIC.to_vec();
        bytes.extend(postcard::to_allocvec(&invalid).unwrap());
        assert_eq!(
            PatchRelink::deserialize(&bytes).unwrap_err(),
            PatchRelinkError::InvalidSignatureLength { actual: 63 }
        );
    }
}

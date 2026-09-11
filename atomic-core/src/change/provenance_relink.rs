//! Signed, content-addressed replacement of provenance for an existing change.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::types::Hash;

const MAGIC: &[u8; 4] = b"PRRL";
const SCHEMA_VERSION: u8 = 1;
const SIGNING_DOMAIN: &[u8] = b"atomic:provenance-relink:v1\0";
const SIGNATURE_LENGTH: usize = 64;

/// An immutable statement replacing the provenance associated with a change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceRelink {
    version: u8,
    old_change: Hash,
    old_provenance: Hash,
    replacement_change: Hash,
    signer: Vec<u8>,
    signed_at: i64,
    signature: Vec<u8>,
}

#[derive(Serialize)]
struct UnsignedProvenanceRelink<'a> {
    version: u8,
    old_change: &'a Hash,
    old_provenance: &'a Hash,
    replacement_change: &'a Hash,
    signer: &'a [u8],
    signed_at: i64,
}

impl ProvenanceRelink {
    /// Constructs a relink after checking its structural invariants.
    pub fn new(
        old_change: Hash,
        old_provenance: Hash,
        replacement_change: Hash,
        signer: Vec<u8>,
        signed_at: i64,
        signature: Vec<u8>,
    ) -> Result<Self, ProvenanceRelinkError> {
        let relink = Self {
            version: SCHEMA_VERSION,
            old_change,
            old_provenance,
            replacement_change,
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

    pub fn old_provenance(&self) -> &Hash {
        &self.old_provenance
    }

    pub fn replacement_change(&self) -> &Hash {
        &self.replacement_change
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

    /// Returns deterministic, domain-separated bytes to sign.
    ///
    /// The signature itself is deliberately excluded.
    pub fn signing_bytes(&self) -> Result<Vec<u8>, ProvenanceRelinkError> {
        let unsigned = UnsignedProvenanceRelink {
            version: self.version,
            old_change: &self.old_change,
            old_provenance: &self.old_provenance,
            replacement_change: &self.replacement_change,
            signer: &self.signer,
            signed_at: self.signed_at,
        };
        let payload = postcard::to_allocvec(&unsigned).map_err(ProvenanceRelinkError::codec)?;
        let mut bytes = Vec::with_capacity(SIGNING_DOMAIN.len() + payload.len());
        bytes.extend_from_slice(SIGNING_DOMAIN);
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// Serializes this relink and returns its content hash.
    pub fn serialize(&self) -> Result<(Vec<u8>, Hash), ProvenanceRelinkError> {
        self.validate()?;
        let payload = postcard::to_allocvec(self).map_err(ProvenanceRelinkError::codec)?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + payload.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&payload);
        let hash = Hash::of(&bytes);
        Ok((bytes, hash))
    }

    /// Deserializes a relink and returns the hash of the exact input bytes.
    pub fn deserialize(data: &[u8]) -> Result<(Self, Hash), ProvenanceRelinkError> {
        if data.len() < MAGIC.len() + 1 {
            return Err(ProvenanceRelinkError::Codec {
                reason: format!("data too short: {} bytes", data.len()),
            });
        }
        if &data[..MAGIC.len()] != MAGIC {
            return Err(ProvenanceRelinkError::Codec {
                reason: "invalid provenance relink magic".into(),
            });
        }
        let relink: Self =
            postcard::from_bytes(&data[MAGIC.len()..]).map_err(ProvenanceRelinkError::codec)?;
        if relink.version != SCHEMA_VERSION {
            return Err(ProvenanceRelinkError::UnsupportedVersion {
                version: relink.version,
                supported: SCHEMA_VERSION,
            });
        }
        relink.validate()?;
        Ok((relink, Hash::of(data)))
    }

    pub fn is_provenance_relink(data: &[u8]) -> bool {
        data.starts_with(MAGIC)
    }

    fn validate(&self) -> Result<(), ProvenanceRelinkError> {
        if self.old_change == self.replacement_change {
            return Err(ProvenanceRelinkError::SameChangeHash);
        }
        if self.signature.len() != SIGNATURE_LENGTH {
            return Err(ProvenanceRelinkError::InvalidSignatureLength {
                actual: self.signature.len(),
            });
        }
        Ok(())
    }
}

/// Errors produced while constructing or decoding a provenance relink.
#[derive(Debug, PartialEq, Eq)]
pub enum ProvenanceRelinkError {
    SameChangeHash,
    InvalidSignatureLength { actual: usize },
    UnsupportedVersion { version: u8, supported: u8 },
    Codec { reason: String },
}

impl ProvenanceRelinkError {
    fn codec(error: impl fmt::Display) -> Self {
        Self::Codec {
            reason: error.to_string(),
        }
    }
}

impl fmt::Display for ProvenanceRelinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SameChangeHash => write!(f, "old and replacement change hashes must differ"),
            Self::InvalidSignatureLength { actual } => write!(
                f,
                "signature must be {SIGNATURE_LENGTH} bytes, got {actual}"
            ),
            Self::UnsupportedVersion { version, supported } => write!(
                f,
                "unsupported provenance relink version {version} (supported: {supported})"
            ),
            Self::Codec { reason } => write!(f, "provenance relink codec error: {reason}"),
        }
    }
}

impl std::error::Error for ProvenanceRelinkError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn relink_with_signature(signature: Vec<u8>) -> ProvenanceRelink {
        ProvenanceRelink::new(
            Hash::of(b"old change"),
            Hash::of(b"old provenance"),
            Hash::of(b"replacement change"),
            vec![1, 2, 3, 4],
            1_735_689_600,
            signature,
        )
        .unwrap()
    }

    fn relink() -> ProvenanceRelink {
        relink_with_signature(vec![7; SIGNATURE_LENGTH])
    }

    #[test]
    fn constructor_and_accessors_preserve_values() {
        let value = relink();
        assert_eq!(value.version(), SCHEMA_VERSION);
        assert_eq!(value.old_change(), &Hash::of(b"old change"));
        assert_eq!(value.old_provenance(), &Hash::of(b"old provenance"));
        assert_eq!(value.replacement_change(), &Hash::of(b"replacement change"));
        assert_eq!(value.signer(), &[1, 2, 3, 4]);
        assert_eq!(value.signed_at(), 1_735_689_600);
        assert_eq!(value.signature(), &[7; SIGNATURE_LENGTH]);
    }

    #[test]
    fn rejects_equal_change_hashes() {
        let hash = Hash::of(b"same");
        let error = ProvenanceRelink::new(
            hash,
            Hash::of(b"provenance"),
            hash,
            vec![],
            0,
            vec![0; SIGNATURE_LENGTH],
        )
        .unwrap_err();
        assert_eq!(error, ProvenanceRelinkError::SameChangeHash);
    }

    #[test]
    fn rejects_every_non_64_byte_signature_length() {
        for length in [0, 1, 63, 65, 128] {
            let error = ProvenanceRelink::new(
                Hash::of(b"old"),
                Hash::of(b"provenance"),
                Hash::of(b"new"),
                vec![],
                0,
                vec![0; length],
            )
            .unwrap_err();
            assert_eq!(
                error,
                ProvenanceRelinkError::InvalidSignatureLength { actual: length }
            );
        }
    }

    #[test]
    fn postcard_roundtrip_returns_hash_of_complete_encoding() {
        let value = relink();
        let (bytes, serialized_hash) = value.serialize().unwrap();
        assert!(ProvenanceRelink::is_provenance_relink(&bytes));
        assert_eq!(&bytes[..4], MAGIC);
        assert_eq!(serialized_hash, Hash::of(&bytes));

        let (decoded, decoded_hash) = ProvenanceRelink::deserialize(&bytes).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(decoded_hash, serialized_hash);
    }

    #[test]
    fn encoding_is_deterministic_and_content_addressed() {
        let (first, first_hash) = relink().serialize().unwrap();
        let (second, second_hash) = relink().serialize().unwrap();
        assert_eq!(first, second);
        assert_eq!(first_hash, second_hash);

        let changed = ProvenanceRelink::new(
            Hash::of(b"old change"),
            Hash::of(b"old provenance"),
            Hash::of(b"replacement change"),
            vec![1, 2, 3, 4],
            1_735_689_601,
            vec![7; SIGNATURE_LENGTH],
        )
        .unwrap();
        let (_, changed_hash) = changed.serialize().unwrap();
        assert_ne!(changed_hash, first_hash);
    }

    #[test]
    fn signing_bytes_are_domain_separated_and_exclude_signature() {
        let first = relink_with_signature(vec![7; SIGNATURE_LENGTH]);
        let second = relink_with_signature(vec![9; SIGNATURE_LENGTH]);
        let first_bytes = first.signing_bytes().unwrap();
        assert!(first_bytes.starts_with(SIGNING_DOMAIN));
        assert_eq!(first_bytes, second.signing_bytes().unwrap());
        assert_ne!(first.serialize().unwrap().0, second.serialize().unwrap().0);
    }

    #[test]
    fn signing_bytes_commit_to_every_unsigned_field() {
        let base = relink();
        let changed_signer = ProvenanceRelink::new(
            *base.old_change(),
            *base.old_provenance(),
            *base.replacement_change(),
            vec![9],
            base.signed_at(),
            base.signature().to_vec(),
        )
        .unwrap();
        assert_ne!(
            base.signing_bytes().unwrap(),
            changed_signer.signing_bytes().unwrap()
        );
    }

    #[test]
    fn rejects_short_data_and_wrong_magic() {
        assert!(matches!(
            ProvenanceRelink::deserialize(b"PRRL"),
            Err(ProvenanceRelinkError::Codec { .. })
        ));
        let (mut bytes, _) = relink().serialize().unwrap();
        bytes[..4].copy_from_slice(b"NOPE");
        assert!(matches!(
            ProvenanceRelink::deserialize(&bytes),
            Err(ProvenanceRelinkError::Codec { .. })
        ));
        assert!(!ProvenanceRelink::is_provenance_relink(&bytes));
    }

    #[test]
    fn rejects_unsupported_version_and_invalid_decoded_values() {
        let mut unsupported = relink();
        unsupported.version = SCHEMA_VERSION + 1;
        let payload = postcard::to_allocvec(&unsupported).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend(payload);
        assert_eq!(
            ProvenanceRelink::deserialize(&bytes).unwrap_err(),
            ProvenanceRelinkError::UnsupportedVersion {
                version: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION,
            }
        );

        let mut invalid = relink();
        invalid.signature.pop();
        let payload = postcard::to_allocvec(&invalid).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend(payload);
        assert_eq!(
            ProvenanceRelink::deserialize(&bytes).unwrap_err(),
            ProvenanceRelinkError::InvalidSignatureLength { actual: 63 }
        );
    }
}

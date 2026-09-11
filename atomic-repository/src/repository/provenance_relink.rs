use std::time::{SystemTime, UNIX_EPOCH};

use atomic_core::change::ProvenanceRelink;
use atomic_core::types::{Base32, Hash};
use atomic_identity::{KeyPair, PublicKey};

use super::{Repository, RepositoryError};

impl Repository {
    /// Create and sign an immutable assertion connecting original provenance to
    /// a replacement change produced by selective repair.
    pub fn create_provenance_relink(
        &self,
        old_change: Hash,
        old_provenance: Hash,
        replacement_change: Hash,
        keypair: &KeyPair,
    ) -> Result<ProvenanceRelink, RepositoryError> {
        let graph = self.load_provenance_graph(&old_provenance)?;
        if !graph.explains_change(&old_change) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "provenance {} does not explain change {}",
                    old_provenance.to_base32(),
                    old_change.to_base32()
                ),
            });
        }
        self.load_change(&old_change)?;
        self.load_change(&replacement_change)?;

        let signed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("system clock precedes Unix epoch: {error}"),
            })?
            .as_secs() as i64;
        let signer = keypair.public.as_bytes().to_vec();
        let unsigned = ProvenanceRelink::new(
            old_change,
            old_provenance,
            replacement_change,
            signer.clone(),
            signed_at,
            vec![0; 64],
        )?;
        let signature = keypair.sign(&unsigned.signing_bytes()?).to_vec();
        let relink = ProvenanceRelink::new(
            old_change,
            old_provenance,
            replacement_change,
            signer,
            signed_at,
            signature,
        )?;
        Self::verify_provenance_relink_signature(&relink)?;
        Ok(relink)
    }

    /// Verify the standalone Ed25519 signature carried by a provenance relink.
    pub fn verify_provenance_relink_signature(
        relink: &ProvenanceRelink,
    ) -> Result<(), RepositoryError> {
        let signer: [u8; 32] =
            relink
                .signer()
                .try_into()
                .map_err(|_| RepositoryError::InvalidOperation {
                    message: format!(
                        "provenance relink signer must be 32 bytes, got {}",
                        relink.signer().len()
                    ),
                })?;
        let signature: [u8; 64] =
            relink
                .signature()
                .try_into()
                .map_err(|_| RepositoryError::InvalidOperation {
                    message: "provenance relink signature must be 64 bytes".to_string(),
                })?;
        let public_key =
            PublicKey::from_bytes(&signer).map_err(|error| RepositoryError::InvalidOperation {
                message: format!("invalid provenance relink signer: {error}"),
            })?;
        public_key
            .verify(&relink.signing_bytes()?, &signature)
            .map_err(|error| RepositoryError::InvalidOperation {
                message: format!("invalid provenance relink signature: {error}"),
            })
    }
}

impl From<atomic_core::change::ProvenanceRelinkError> for RepositoryError {
    fn from(error: atomic_core::change::ProvenanceRelinkError) -> Self {
        RepositoryError::InvalidOperation {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_signed_relink_and_rejects_tampering() {
        let keypair = KeyPair::generate();
        let old = Hash::of(b"old");
        let provenance = Hash::of(b"provenance");
        let replacement = Hash::of(b"replacement");
        let unsigned = ProvenanceRelink::new(
            old,
            provenance,
            replacement,
            keypair.public.as_bytes().to_vec(),
            42,
            vec![0; 64],
        )
        .unwrap();
        let signature = keypair.sign(&unsigned.signing_bytes().unwrap()).to_vec();
        let relink = ProvenanceRelink::new(
            old,
            provenance,
            replacement,
            keypair.public.as_bytes().to_vec(),
            42,
            signature.clone(),
        )
        .unwrap();
        Repository::verify_provenance_relink_signature(&relink).unwrap();

        let tampered = ProvenanceRelink::new(
            old,
            provenance,
            Hash::of(b"other replacement"),
            keypair.public.as_bytes().to_vec(),
            42,
            signature,
        )
        .unwrap();
        assert!(Repository::verify_provenance_relink_signature(&tampered).is_err());
    }
}

use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use super::{AttestationEnvelope, AttestedPayload};

pub struct MockAttestationAuthority {
    pub signing_key: SigningKey,
}

impl MockAttestationAuthority {
    pub fn new() -> Self {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        Self { signing_key }
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn create_attested_payload(
        &self,
        url: &str,
        body: &[u8],
        fetch_timestamp: i64,
        headers: Vec<(String, String)>,
    ) -> AttestedPayload {
        let mut hasher = Sha256::new();
        hasher.update(body);
        let payload_sha256: [u8; 32] = hasher.finalize().into();

        let mut canonical_headers = headers;
        canonical_headers.sort_by(|a, b| a.0.cmp(&b.0));

        let mut payload = AttestedPayload {
            raw_bytes: body.to_vec(),
            payload_sha256,
            canonical_headers,
            source_url: url.to_string(),
            fetch_timestamp,
            rfc3161_timestamp_token: None,
            server_tls_cert_sha256: Some([7u8; 32]),
            attestation: AttestationEnvelope::Ed25519Authority {
                authority_pubkey: self.public_key_bytes(),
                signature: Vec::new(),
            },
        };

        let commitment = payload.commitment_hash();
        let sig = self.signing_key.sign(&commitment);

        payload.attestation = AttestationEnvelope::Ed25519Authority {
            authority_pubkey: self.public_key_bytes(),
            signature: sig.to_bytes().to_vec(),
        };

        payload
    }
}

pub mod http;
pub mod mock;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum ProvenanceError {
    #[error("Payload hash mismatch: computed {computed}, declared {declared}")]
    PayloadHashMismatch { computed: String, declared: String },
    #[error("Cryptographic signature invalid: {0}")]
    InvalidSignature(String),
    #[error("Public key format invalid: {0}")]
    InvalidPublicKey(String),
    #[error("Missing required header: {0}")]
    MissingHeader(String),
    #[error("Provider network error: {0}")]
    NetworkError(String),
    #[error(
        "Timestamp outside acceptable drift: delta {delta_seconds}s > allowed {allowed_seconds}s"
    )]
    TimestampDriftExceeded {
        delta_seconds: i64,
        allowed_seconds: i64,
    },
    #[error("Attestation kind unsupported or corrupt: {0}")]
    UnsupportedAttestation(String),
}

/// Cryptographic attestation envelope proving byte provenance
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "proof")]
pub enum AttestationEnvelope {
    /// Zero-Knowledge TLS Notary Session Proof (e.g., TLSNotary MPC commitment)
    TlsNotarySession {
        session_id: String,
        notary_pubkey: [u8; 32],
        notary_signature: Vec<u8>,
        server_name: String,
    },
    /// Hardware Trusted Execution Environment Attestation (e.g. AWS Nitro Enclave PCRs)
    NitroEnclave {
        pcr0: String,
        pcr1: String,
        pcr2: String,
        enclave_pubkey: [u8; 32],
        enclave_signature: Vec<u8>,
    },
    /// Direct Hardware/Authority Ed25519 Attestation
    Ed25519Authority {
        authority_pubkey: [u8; 32],
        signature: Vec<u8>,
    },
}

/// Raw byte-level verifiable HTTP payload with canonical headers & cryptographic proof
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttestedPayload {
    /// Exact raw wire bytes of response body
    pub raw_bytes: Vec<u8>,
    /// SHA-256 of raw_bytes (computed: sha256(raw_bytes))
    pub payload_sha256: [u8; 32],
    /// Sorted, lowercase HTTP response headers (e.g. [("date", "..."), ("etag", "...")])
    pub canonical_headers: Vec<(String, String)>,
    /// Canonical source URL
    pub source_url: String,
    /// Unix timestamp (seconds) when payload was fetched/attested
    pub fetch_timestamp: i64,
    /// RFC 3161 Time-Stamp Token bytes, if present
    pub rfc3161_timestamp_token: Option<Vec<u8>>,
    /// TLS certificate fingerprint of server
    pub server_tls_cert_sha256: Option<[u8; 32]>,
    /// Cryptographic attestation envelope
    pub attestation: AttestationEnvelope,
}

impl AttestedPayload {
    /// Canonical commitment hash over (url + timestamp + payload_sha256)
    pub fn commitment_hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"RESOLUTION_SERVICE_ATTESTATION_V1:");
        hasher.update(self.source_url.as_bytes());
        hasher.update(self.fetch_timestamp.to_be_bytes());
        hasher.update(self.payload_sha256);

        // Canonical headers commitment
        for (k, v) in &self.canonical_headers {
            hasher.update(k.as_bytes());
            hasher.update(b":");
            hasher.update(v.as_bytes());
            hasher.update(b"\n");
        }

        let res = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&res);
        out
    }

    /// Verifies that raw_bytes matches payload_sha256 and cryptographic proof is authentic.
    /// Runs 100% offline with zero network or database dependencies.
    pub fn verify_integrity(&self) -> Result<(), ProvenanceError> {
        // 1. Verify byte hash
        let mut hasher = Sha256::new();
        hasher.update(&self.raw_bytes);
        let computed = hasher.finalize();
        if computed.as_slice() != self.payload_sha256.as_slice() {
            return Err(ProvenanceError::PayloadHashMismatch {
                computed: hex::encode(computed),
                declared: hex::encode(self.payload_sha256),
            });
        }

        // 2. Verify cryptographic signature over commitment hash
        let commitment = self.commitment_hash();

        match &self.attestation {
            AttestationEnvelope::Ed25519Authority {
                authority_pubkey,
                signature,
            } => {
                let verifying_key = VerifyingKey::from_bytes(authority_pubkey)
                    .map_err(|e| ProvenanceError::InvalidPublicKey(e.to_string()))?;

                let sig = Signature::from_slice(signature)
                    .map_err(|e| ProvenanceError::InvalidSignature(e.to_string()))?;

                verifying_key
                    .verify(&commitment, &sig)
                    .map_err(|e| ProvenanceError::InvalidSignature(e.to_string()))?;
            }
            AttestationEnvelope::NitroEnclave {
                enclave_pubkey,
                enclave_signature,
                pcr0,
                ..
            } => {
                if pcr0.is_empty() {
                    return Err(ProvenanceError::UnsupportedAttestation(
                        "Missing PCR0 measurement".into(),
                    ));
                }
                let verifying_key = VerifyingKey::from_bytes(enclave_pubkey)
                    .map_err(|e| ProvenanceError::InvalidPublicKey(e.to_string()))?;
                let sig = Signature::from_slice(enclave_signature)
                    .map_err(|e| ProvenanceError::InvalidSignature(e.to_string()))?;
                verifying_key
                    .verify(&commitment, &sig)
                    .map_err(|e| ProvenanceError::InvalidSignature(e.to_string()))?;
            }
            AttestationEnvelope::TlsNotarySession {
                notary_pubkey,
                notary_signature,
                server_name,
                ..
            } => {
                if !self.source_url.contains(server_name) {
                    return Err(ProvenanceError::UnsupportedAttestation(format!(
                        "Server name '{}' not in source URL '{}'",
                        server_name, self.source_url
                    )));
                }
                let verifying_key = VerifyingKey::from_bytes(notary_pubkey)
                    .map_err(|e| ProvenanceError::InvalidPublicKey(e.to_string()))?;
                let sig = Signature::from_slice(notary_signature)
                    .map_err(|e| ProvenanceError::InvalidSignature(e.to_string()))?;
                verifying_key
                    .verify(&commitment, &sig)
                    .map_err(|e| ProvenanceError::InvalidSignature(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Extract header by name
    pub fn get_header(&self, name: &str) -> Option<&str> {
        let name_lower = name.to_lowercase();
        self.canonical_headers
            .iter()
            .find(|(k, _)| k == &name_lower)
            .map(|(_, v)| v.as_str())
    }
}

/// Abstract provider interface for fetching attested payloads
#[async_trait::async_trait]
pub trait AttestedPayloadProvider: Send + Sync {
    async fn fetch_attested(&self, url: &str) -> Result<AttestedPayload, ProvenanceError>;
}

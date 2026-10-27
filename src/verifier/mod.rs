use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ast::{AstError, EvaluationContext, Value};
use crate::blueprints::CompiledMarketSpec;
use crate::provenance::{AttestedPayload, ProvenanceError};

#[derive(Error, Debug, PartialEq, Clone)]
pub enum VerifierError {
    #[error("Cryptographic provenance check failed: {0}")]
    ProvenanceError(#[from] ProvenanceError),
    #[error("Source URL mismatch: expected '{expected}', attested '{attested}'")]
    SourceUrlMismatch { expected: String, attested: String },
    #[error("Attestation timestamp {attested_time} occurred before event cutoff {cutoff_time}")]
    PrematureAttestation { attested_time: i64, cutoff_time: i64 },
    #[error("Attestation timestamp {attested_time} exceeds future clock skew against current time {current_time}")]
    FutureTimestamp { attested_time: i64, current_time: i64 },
    #[error("AST evaluation failure: {0}")]
    AstError(#[from] AstError),
    #[error("Evaluated outcome is not boolean: {0}")]
    NonBooleanOutcome(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifiedResolutionReport {
    pub market_id: String,
    /// 1 = YES, 2 = NO, 3 = VOID
    pub outcome: u8,
    pub evaluated_value: Value,
    pub payload_commitment: [u8; 32],
    pub source_url: String,
    pub attested_at: i64,
    pub verified_at: i64,
}

/// Standalone, pure offline verification function.
/// Takes zero database or network dependencies.
pub fn verify_resolution_offline(
    market_id: &str,
    spec: &CompiledMarketSpec,
    attestation: &AttestedPayload,
    current_time: i64,
) -> Result<VerifiedResolutionReport, VerifierError> {
    // 1. Cryptographic proof and byte integrity verification
    attestation.verify_integrity()?;

    // 2. Canonical URL validation
    let is_canonical_source = attestation.source_url == spec.primary_source_url
        || spec.secondary_source_urls.iter().any(|u| u == &attestation.source_url);

    if !is_canonical_source {
        return Err(VerifierError::SourceUrlMismatch {
            expected: spec.primary_source_url.clone(),
            attested: attestation.source_url.clone(),
        });
    }

    // 3. Temporal invariant: Attestation must be captured at or after cutoff
    if attestation.fetch_timestamp < spec.temporal_rules.cutoff_time {
        return Err(VerifierError::PrematureAttestation {
            attested_time: attestation.fetch_timestamp,
            cutoff_time: spec.temporal_rules.cutoff_time,
        });
    }

    // Temporal invariant: Attestation cannot be from the future (> 60s clock skew)
    if attestation.fetch_timestamp > current_time + 60 {
        return Err(VerifierError::FutureTimestamp {
            attested_time: attestation.fetch_timestamp,
            current_time,
        });
    }

    // 4. Pure AST evaluation over attested raw bytes
    let raw_text = String::from_utf8_lossy(&attestation.raw_bytes);
    let ctx = EvaluationContext::new(raw_text);

    let evaluated = spec.ast_expression.evaluate(&ctx)?;
    let outcome_bool = evaluated.as_bool().map_err(|e| VerifierError::NonBooleanOutcome(e.to_string()))?;

    let outcome_code = if outcome_bool { 1 } else { 2 };

    Ok(VerifiedResolutionReport {
        market_id: market_id.to_string(),
        outcome: outcome_code,
        evaluated_value: evaluated,
        payload_commitment: attestation.commitment_hash(),
        source_url: attestation.source_url.clone(),
        attested_at: attestation.fetch_timestamp,
        verified_at: Utc::now().timestamp(),
    })
}

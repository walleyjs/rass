pub mod fixture;
pub mod predicates;
pub mod templates;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::provenance::{AttestedPayload, ProvenanceError};

pub use fixture::{
    normalize_athlete_name, AthleteParticipation, ConfirmedScoringPlay, FixtureParseError,
    HomeAway, MatchClock, MatchLifecycleStatus, NormalizedFixtureState, ParticipationStatus,
    TeamScoreBreakdown,
};
pub use predicates::{
    AthleteFilter, MatchSelection, OverUnderSide, ParticipationPolicy, SettlementScope,
    SportsEvaluationConfig, SportsMarketPredicate, SportsResolutionDecision,
};
pub use templates::{FixtureMarketMatrix, SportsMarketTemplate};

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum SportsResolutionError {
    #[error("Cryptographic attestation verification failed: {0}")]
    Provenance(#[from] ProvenanceError),
    #[error("Fixture JSON normalization failed: {0}")]
    FixtureParse(#[from] FixtureParseError),
    #[error("Report commitment mismatch: expected {expected}, found {found}")]
    ReportCommitmentMismatch { expected: String, found: String },
    #[error("Verdict mismatch for market '{market_id}'")]
    VerdictMismatch { market_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketResolutionVerdict {
    pub market_id: String,
    pub template_key: String,
    pub question: String,
    pub decision: SportsResolutionDecision,
    /// 1 = YES, 2 = NO, 3 = VOID, 0 = PENDING
    pub outcome_code: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiplexedFixtureResolutionReport {
    pub event_id: String,
    pub sport_path: String,
    pub lifecycle_status: MatchLifecycleStatus,
    pub payload_commitment: [u8; 32],
    pub report_merkle_commitment: [u8; 32],
    pub attested_at: i64,
    pub verdicts: Vec<MarketResolutionVerdict>,
}

impl MultiplexedFixtureResolutionReport {
    pub fn compute_report_commitment(
        event_id: &str,
        payload_commitment: &[u8; 32],
        verdicts: &[MarketResolutionVerdict],
    ) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"SPORTS_MULTIPLEXED_REPORT_V1:");
        hasher.update(event_id.as_bytes());
        hasher.update(payload_commitment);
        for v in verdicts {
            hasher.update(v.market_id.as_bytes());
            hasher.update([v.outcome_code]);
        }
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

#[derive(Default)]
pub struct MultiplexedSportsResolver {
    pub config: SportsEvaluationConfig,
}

impl MultiplexedSportsResolver {
    pub fn new(config: SportsEvaluationConfig) -> Self {
        Self { config }
    }

    /// Evaluates an entire matrix of derivative sports markets against a single cryptographically
    /// attested ESPN summary payload.
    /// 1. Verifies the cryptographic signature and SHA-256 byte integrity once.
    /// 2. Normalizes the fixture JSON once.
    /// 3. Validates team entities and evaluates all N market predicates in a single atomic pass.
    pub fn resolve_matrix(
        &self,
        sport_path: &str,
        event_id: &str,
        attestation: &AttestedPayload,
        templates: &[SportsMarketTemplate],
    ) -> Result<MultiplexedFixtureResolutionReport, SportsResolutionError> {
        // 1. Cryptographic integrity check (runs once for all N markets)
        attestation.verify_integrity()?;

        // 2. Parse & normalize fixture state (runs once for all N markets)
        let raw_json = String::from_utf8_lossy(&attestation.raw_bytes);
        let fixture =
            NormalizedFixtureState::from_espn_summary_json(sport_path, event_id, &raw_json)?;

        let payload_commitment = attestation.commitment_hash();
        let mut verdicts = Vec::with_capacity(templates.len());

        // 3. Evaluate each derivative market template
        for tmpl in templates {
            // Entity validation guard
            fixture.validate_entities(&tmpl.question)?;

            let decision =
                tmpl.predicate
                    .evaluate(&fixture, attestation.fetch_timestamp, &self.config);

            let outcome_code = match &decision {
                SportsResolutionDecision::ResolvedYes { .. } => 1,
                SportsResolutionDecision::ResolvedNo { .. } => 2,
                SportsResolutionDecision::Void { code, .. } => *code,
                SportsResolutionDecision::Pending { .. } => 0,
            };

            verdicts.push(MarketResolutionVerdict {
                market_id: tmpl.market_id.clone(),
                template_key: tmpl.template_key.clone(),
                question: tmpl.question.clone(),
                decision,
                outcome_code,
            });
        }

        let report_merkle_commitment =
            MultiplexedFixtureResolutionReport::compute_report_commitment(
                &fixture.event_id,
                &payload_commitment,
                &verdicts,
            );

        Ok(MultiplexedFixtureResolutionReport {
            event_id: fixture.event_id,
            sport_path: fixture.sport_path,
            lifecycle_status: fixture.lifecycle_status,
            payload_commitment,
            report_merkle_commitment,
            attested_at: attestation.fetch_timestamp,
            verdicts,
        })
    }

    /// Standalone, air-gapped verification of a multiplexed sports resolution report.
    pub fn verify_report_offline(
        &self,
        attestation: &AttestedPayload,
        templates: &[SportsMarketTemplate],
        expected_report: &MultiplexedFixtureResolutionReport,
    ) -> Result<(), SportsResolutionError> {
        let recomputed = self.resolve_matrix(
            &expected_report.sport_path,
            &expected_report.event_id,
            attestation,
            templates,
        )?;

        if recomputed.report_merkle_commitment != expected_report.report_merkle_commitment {
            return Err(SportsResolutionError::ReportCommitmentMismatch {
                expected: hex::encode(expected_report.report_merkle_commitment),
                found: hex::encode(recomputed.report_merkle_commitment),
            });
        }

        for (actual, expected) in recomputed
            .verdicts
            .iter()
            .zip(expected_report.verdicts.iter())
        {
            if actual != expected {
                return Err(SportsResolutionError::VerdictMismatch {
                    market_id: expected.market_id.clone(),
                });
            }
        }

        Ok(())
    }
}

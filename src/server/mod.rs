use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::blueprints::{CompiledMarketSpec, MarketBlueprint};
use crate::fsm::{Bond, EscalationLadder, MarketLifecycleState, ResolutionFsm};
use crate::ledger::{AuditLedger, LedgerEntry, LedgerEvent};
use crate::provenance::AttestedPayload;
use crate::receipt::SettlementReceipt;
use crate::scheduler::{CoalescedPollScheduler, ConfirmationStepResult, ConfirmationTracker};
use crate::sports::{
    FixtureMarketMatrix, MatchLifecycleStatus, NormalizedFixtureState, SportsEvaluationConfig,
    SportsMarketTemplate,
};

pub struct RaasEngine {
    pub signing_key: SigningKey,
    pub ledger: AuditLedger,
    pub scheduler: CoalescedPollScheduler,
    pub confirmation_tracker: ConfirmationTracker,
    pub sports_eval_config: SportsEvaluationConfig,
    pub fsms: HashMap<String, ResolutionFsm>,
    pub general_specs: HashMap<String, CompiledMarketSpec>,
    pub sports_matrices: HashMap<String, Vec<SportsMarketTemplate>>,
    pub latest_commitments: HashMap<String, [u8; 32]>,
    pub receipts: HashMap<String, SettlementReceipt>,
}

impl RaasEngine {
    pub fn new(confirmation_depth_seconds: i64) -> Self {
        let mut csprng = OsRng;
        let signing_key = SigningKey::generate(&mut csprng);
        Self {
            signing_key,
            ledger: AuditLedger::new(),
            scheduler: CoalescedPollScheduler::new(),
            confirmation_tracker: ConfirmationTracker::new(confirmation_depth_seconds),
            sports_eval_config: SportsEvaluationConfig::default(),
            fsms: HashMap::new(),
            general_specs: HashMap::new(),
            sports_matrices: HashMap::new(),
            latest_commitments: HashMap::new(),
            receipts: HashMap::new(),
        }
    }

    pub fn register_blueprint(
        &mut self,
        market_id: String,
        blueprint: MarketBlueprint,
        registered_at: i64,
    ) -> CompiledMarketSpec {
        let spec = blueprint.compile();
        let fsm = ResolutionFsm::new(
            market_id.clone(),
            spec.temporal_rules.clone(),
            EscalationLadder::default(),
        );
        self.fsms.insert(market_id.clone(), fsm);
        self.scheduler.schedule_market(
            market_id.clone(),
            spec.primary_source_url.clone(),
            spec.temporal_rules.cutoff_time,
        );
        self.ledger.append(
            registered_at,
            LedgerEvent::MarketRegistered {
                market_id: market_id.clone(),
                origin_key: spec.primary_source_url.clone(),
                question: spec.question.clone(),
            },
        );
        self.general_specs.insert(market_id, spec.clone());
        spec
    }

    pub fn register_sports_fixture_matrix(
        &mut self,
        sport_path: &str,
        event_id: &str,
        raw_fixture_json: &str,
        registered_at: i64,
    ) -> Result<FixtureMarketMatrix, String> {
        let fixture =
            NormalizedFixtureState::from_espn_summary_json(sport_path, event_id, raw_fixture_json)
                .map_err(|e| e.to_string())?;
        let matrix = FixtureMarketMatrix::generate_from_fixture(&fixture);
        let origin_key = format!("{}:{}", matrix.sport_path, matrix.event_id);

        for tmpl in &matrix.templates {
            let fsm = ResolutionFsm::new(
                tmpl.market_id.clone(),
                tmpl.temporal_rules.clone(),
                EscalationLadder::default(),
            );
            self.fsms.insert(tmpl.market_id.clone(), fsm);
            self.scheduler.schedule_market(
                tmpl.market_id.clone(),
                origin_key.clone(),
                tmpl.initial_poll_after,
            );
            self.ledger.append(
                registered_at,
                LedgerEvent::MarketRegistered {
                    market_id: tmpl.market_id.clone(),
                    origin_key: origin_key.clone(),
                    question: tmpl.question.clone(),
                },
            );
        }

        self.sports_matrices
            .insert(matrix.event_id.clone(), matrix.templates.clone());
        Ok(matrix)
    }

    /// Ingests a signed AttestedPayload for a sports fixture, runs multiplexed predicate evaluation,
    /// applies stateful multi-snapshot confirmation depth, and advances FSM + AuditLedger.
    pub fn ingest_sports_snapshot(
        &mut self,
        sport_path: &str,
        event_id: &str,
        attestation: &AttestedPayload,
    ) -> Result<Vec<(String, ConfirmationStepResult)>, String> {
        // 1. Verify cryptographic attestation once
        attestation.verify_integrity().map_err(|e| e.to_string())?;

        // 2. Parse normalized fixture state once
        let raw_json = String::from_utf8_lossy(&attestation.raw_bytes);
        let fixture =
            NormalizedFixtureState::from_espn_summary_json(sport_path, event_id, &raw_json)
                .map_err(|e| e.to_string())?;

        let templates = self
            .sports_matrices
            .get(event_id)
            .ok_or_else(|| format!("No registered market matrix for event_id '{}'", event_id))?
            .clone();

        let payload_commitment = attestation.commitment_hash();
        let snapshot_ts = attestation.fetch_timestamp;
        let is_in_play = fixture.lifecycle_status == MatchLifecycleStatus::InPlay;

        let mut results = Vec::with_capacity(templates.len());

        for tmpl in &templates {
            let mid = &tmpl.market_id;

            // Skip markets that have already been proposed, disputed, finalized, or voided
            let is_active = self
                .fsms
                .get(mid)
                .map(|f| matches!(f.state, MarketLifecycleState::Active))
                .unwrap_or(false);
            if !is_active {
                continue;
            }

            fixture
                .validate_entities(&tmpl.question)
                .map_err(|e| e.to_string())?;

            let raw_decision =
                tmpl.predicate
                    .evaluate(&fixture, snapshot_ts, &self.sports_eval_config);

            let step = self.confirmation_tracker.process_decision(
                mid,
                &raw_decision,
                is_in_play,
                snapshot_ts,
                payload_commitment,
            );

            match &step {
                ConfirmationStepResult::TentativeObserved {
                    outcome_code,
                    first_seen_timestamp,
                    next_poll_timestamp,
                    ..
                } => {
                    self.scheduler
                        .update_market_poll_horizon(mid, *next_poll_timestamp);
                    self.ledger.append(
                        snapshot_ts,
                        LedgerEvent::TentativeOutcomeObserved {
                            market_id: mid.clone(),
                            outcome_code: *outcome_code,
                            first_seen_at: *first_seen_timestamp,
                            payload_commitment,
                        },
                    );
                }
                ConfirmationStepResult::AwaitingConfirmationDepth {
                    next_poll_timestamp,
                    ..
                } => {
                    self.scheduler
                        .update_market_poll_horizon(mid, *next_poll_timestamp);
                }
                ConfirmationStepResult::CandidateReverted {
                    reverted_outcome_code,
                    reverted_at,
                    next_poll_timestamp,
                    reason,
                } => {
                    self.scheduler
                        .update_market_poll_horizon(mid, *next_poll_timestamp);
                    self.ledger.append(
                        snapshot_ts,
                        LedgerEvent::TentativeOutcomeReverted {
                            market_id: mid.clone(),
                            reverted_outcome_code: *reverted_outcome_code,
                            reverted_at: *reverted_at,
                            payload_commitment,
                            reason: reason.clone(),
                        },
                    );
                }
                ConfirmationStepResult::StillPending {
                    next_poll_timestamp,
                    ..
                } => {
                    self.scheduler
                        .update_market_poll_horizon(mid, *next_poll_timestamp);
                }
                ConfirmationStepResult::Confirmed {
                    outcome_code,
                    confirmed_at,
                    payload_commitment,
                    ..
                } => {
                    if let Some(fsm) = self.fsms.get_mut(mid) {
                        let bond_amt = fsm.ladder.initial_bond;
                        let bond = Bond {
                            currency: "USDC".into(),
                            amount: bond_amt,
                            bonded_party: "raas_oracle_authority".into(),
                        };
                        let propose_ts = (*confirmed_at).max(fsm.temporal_rules.cutoff_time);
                        fsm.propose(*outcome_code, bond, *payload_commitment, propose_ts)
                            .map_err(|e| e.to_string())?;

                        let expires_at = propose_ts + fsm.temporal_rules.challenge_window_seconds;
                        self.latest_commitments
                            .insert(mid.clone(), *payload_commitment);
                        self.scheduler.complete_market(mid);

                        self.ledger.append(
                            propose_ts,
                            LedgerEvent::OutcomeProposed {
                                market_id: mid.clone(),
                                outcome_code: *outcome_code,
                                proposed_at: propose_ts,
                                expires_at,
                                bond_amount: bond_amt,
                                payload_commitment: *payload_commitment,
                            },
                        );
                    }
                }
                ConfirmationStepResult::ConfirmedVoid {
                    code,
                    reason,
                    payload_commitment,
                } => {
                    if let Some(fsm) = self.fsms.get_mut(mid) {
                        fsm.void_market(*code, reason.clone(), snapshot_ts)
                            .map_err(|e| e.to_string())?;
                        self.latest_commitments
                            .insert(mid.clone(), *payload_commitment);
                        self.scheduler.complete_market(mid);

                        let head_before = self.ledger.head_hash();
                        let receipt = SettlementReceipt::issue(
                            &self.signing_key,
                            mid.clone(),
                            *code,
                            *payload_commitment,
                            head_before,
                            snapshot_ts,
                        )
                        .map_err(|e| e.to_string())?;

                        self.ledger.append(
                            snapshot_ts,
                            LedgerEvent::MarketVoided {
                                market_id: mid.clone(),
                                void_code: *code,
                                reason: reason.clone(),
                                voided_at: snapshot_ts,
                                receipt_hash: receipt.receipt_hash(),
                            },
                        );
                        self.receipts.insert(mid.clone(), receipt);
                    }
                }
            }

            results.push((mid.clone(), step));
        }

        Ok(results)
    }

    pub fn challenge_market(
        &mut self,
        market_id: &str,
        counter_outcome: u8,
        bond: Bond,
        current_time: i64,
    ) -> Result<MarketLifecycleState, String> {
        let fsm = self
            .fsms
            .get_mut(market_id)
            .ok_or_else(|| format!("Market '{}' not found", market_id))?;

        let bond_amt = bond.amount;
        let challenger = bond.bonded_party.clone();
        fsm.challenge(counter_outcome, bond, current_time)
            .map_err(|e| e.to_string())?;

        let round = match &fsm.state {
            MarketLifecycleState::Disputed { round, .. } => *round,
            _ => 1,
        };

        self.ledger.append(
            current_time,
            LedgerEvent::DisputeChallenged {
                market_id: market_id.to_string(),
                round,
                counter_outcome,
                bond_amount: bond_amt,
                challenger,
            },
        );

        Ok(fsm.state.clone())
    }

    pub fn finalize_market(
        &mut self,
        market_id: &str,
        current_time: i64,
    ) -> Result<SettlementReceipt, String> {
        let fsm = self
            .fsms
            .get_mut(market_id)
            .ok_or_else(|| format!("Market '{}' not found", market_id))?;

        let final_outcome = fsm.finalize(current_time).map_err(|e| e.to_string())?;
        let attestation_commitment = self
            .latest_commitments
            .get(market_id)
            .copied()
            .unwrap_or([0u8; 32]);
        let ledger_head = self.ledger.head_hash();

        let receipt = SettlementReceipt::issue(
            &self.signing_key,
            market_id.to_string(),
            final_outcome,
            attestation_commitment,
            ledger_head,
            current_time,
        )
        .map_err(|e| e.to_string())?;

        self.ledger.append(
            current_time,
            LedgerEvent::MarketFinalized {
                market_id: market_id.to_string(),
                final_outcome,
                finalized_at: current_time,
                receipt_hash: receipt.receipt_hash(),
            },
        );

        self.receipts.insert(market_id.to_string(), receipt.clone());
        Ok(receipt)
    }
}

// -----------------------------------------------------------------------------
// HTTP DTOs & Axum Handlers
// -----------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterBlueprintRequest {
    pub market_id: String,
    pub blueprint: MarketBlueprint,
    pub registered_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterFixtureMatrixRequest {
    pub sport_path: String,
    pub event_id: String,
    pub raw_fixture_json: String,
    pub registered_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IngestFixtureSnapshotRequest {
    pub sport_path: String,
    pub attestation: AttestedPayload,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IngestFixtureSnapshotResponse {
    pub event_id: String,
    pub evaluations: Vec<(String, ConfirmationStepResult)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChallengeMarketRequest {
    pub counter_outcome: u8,
    pub bond: Bond,
    pub current_time: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FinalizeMarketRequest {
    pub current_time: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LedgerAuditResponse {
    pub valid: bool,
    pub entry_count: usize,
    pub head_hash: String,
    pub entries: Vec<LedgerEntry>,
}

pub fn build_router(state: Arc<RwLock<RaasEngine>>) -> Router {
    Router::new()
        .route("/v1/markets/blueprint", post(handle_register_blueprint))
        .route(
            "/v1/sports/fixtures/matrix",
            post(handle_register_fixture_matrix),
        )
        .route(
            "/v1/sports/fixtures/:event_id/ingest",
            post(handle_ingest_fixture_snapshot),
        )
        .route(
            "/v1/markets/:market_id/challenge",
            post(handle_challenge_market),
        )
        .route(
            "/v1/markets/:market_id/finalize",
            post(handle_finalize_market),
        )
        .route("/v1/markets/:market_id/receipt", get(handle_get_receipt))
        .route("/v1/ledger/audit", get(handle_ledger_audit))
        .with_state(state)
}

async fn handle_register_blueprint(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Json(req): Json<RegisterBlueprintRequest>,
) -> impl IntoResponse {
    let mut engine = state.write().await;
    let spec = engine.register_blueprint(req.market_id, req.blueprint, req.registered_at);
    (StatusCode::CREATED, Json(spec))
}

async fn handle_register_fixture_matrix(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Json(req): Json<RegisterFixtureMatrixRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut engine = state.write().await;
    let matrix = engine
        .register_sports_fixture_matrix(
            &req.sport_path,
            &req.event_id,
            &req.raw_fixture_json,
            req.registered_at,
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok((StatusCode::CREATED, Json(matrix)))
}

async fn handle_ingest_fixture_snapshot(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Path(event_id): Path<String>,
    Json(req): Json<IngestFixtureSnapshotRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut engine = state.write().await;
    let evaluations = engine
        .ingest_sports_snapshot(&req.sport_path, &event_id, &req.attestation)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok((
        StatusCode::OK,
        Json(IngestFixtureSnapshotResponse {
            event_id,
            evaluations,
        }),
    ))
}

async fn handle_challenge_market(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Path(market_id): Path<String>,
    Json(req): Json<ChallengeMarketRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut engine = state.write().await;
    let new_state = engine
        .challenge_market(&market_id, req.counter_outcome, req.bond, req.current_time)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok((StatusCode::OK, Json(new_state)))
}

async fn handle_finalize_market(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Path(market_id): Path<String>,
    Json(req): Json<FinalizeMarketRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut engine = state.write().await;
    let receipt = engine
        .finalize_market(&market_id, req.current_time)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok((StatusCode::OK, Json(receipt)))
}

async fn handle_get_receipt(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Path(market_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let engine = state.read().await;
    let receipt = engine
        .receipts
        .get(&market_id)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("No receipt for {}", market_id)))?;
    Ok((StatusCode::OK, Json(receipt)))
}

async fn handle_ledger_audit(
    State(state): State<Arc<RwLock<RaasEngine>>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let engine = state.read().await;
    engine
        .ledger
        .verify_chain_integrity()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let resp = LedgerAuditResponse {
        valid: true,
        entry_count: engine.ledger.entries.len(),
        head_hash: hex::encode(engine.ledger.head_hash()),
        entries: engine.ledger.entries.clone(),
    };
    Ok((StatusCode::OK, Json(resp)))
}

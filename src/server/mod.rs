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
use std::time::Duration;
use tokio::sync::RwLock;

use crate::blueprints::{CompiledMarketSpec, MarketBlueprint};
use crate::fsm::{Bond, EscalationLadder, MarketLifecycleState, ResolutionFsm};
use crate::ledger::{AuditLedger, LedgerEntry, LedgerEvent};
use crate::provenance::http::{DiscoveredEspnEvent, HttpAttestingProvider};
use crate::provenance::{AttestedPayload, AttestedPayloadProvider};
use crate::receipt::SettlementReceipt;
use crate::scheduler::{CoalescedPollScheduler, ConfirmationStepResult, ConfirmationTracker};
use crate::sports::{
    FixtureMarketMatrix, MatchLifecycleStatus, NormalizedFixtureState, SportsEvaluationConfig,
    SportsMarketTemplate,
};
use crate::verifier::{verify_resolution_offline, VerifiedResolutionReport};

pub struct RaasEngine {
    pub signing_key: SigningKey,
    pub http_provider: HttpAttestingProvider,
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
        let http_provider = HttpAttestingProvider::with_signing_key(
            signing_key.clone(),
            Duration::from_secs(120),
            Duration::from_millis(300),
        );
        Self {
            signing_key,
            http_provider,
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

    pub fn list_markets(&self) -> Vec<MarketSummaryDto> {
        let mut summaries = Vec::with_capacity(self.fsms.len());

        for (mid, fsm) in &self.fsms {
            let (question, origin_key) = if let Some(spec) = self.general_specs.get(mid) {
                (spec.question.clone(), spec.primary_source_url.clone())
            } else {
                let mut found = ("Sports Derivative Market".to_string(), "espn".to_string());
                for (event_id, templates) in &self.sports_matrices {
                    if let Some(t) = templates.iter().find(|t| &t.market_id == mid) {
                        found = (t.question.clone(), format!("espn:{}", event_id));
                        break;
                    }
                }
                found
            };

            summaries.push(MarketSummaryDto {
                market_id: mid.clone(),
                question,
                origin_key,
                state: fsm.state.clone(),
                has_receipt: self.receipts.contains_key(mid),
            });
        }

        summaries.sort_by(|a, b| a.market_id.cmp(&b.market_id));
        summaries
    }

    /// Evaluates a cryptographically attested HTTP payload against a compiled general AST market spec,
    /// proposes the resulting outcome on the market's FSM, and appends an `OutcomeProposed` entry to the ledger.
    pub fn ingest_general_attestation(
        &mut self,
        market_id: &str,
        attestation: &AttestedPayload,
    ) -> Result<(VerifiedResolutionReport, MarketLifecycleState), String> {
        let spec = self
            .general_specs
            .get(market_id)
            .ok_or_else(|| format!("General market '{}' not found", market_id))?
            .clone();

        let eval_time = attestation
            .fetch_timestamp
            .max(spec.temporal_rules.cutoff_time);

        let report = verify_resolution_offline(market_id, &spec, attestation, eval_time)
            .map_err(|e| e.to_string())?;

        let fsm = self
            .fsms
            .get_mut(market_id)
            .ok_or_else(|| format!("FSM for market '{}' not found", market_id))?;

        if matches!(fsm.state, MarketLifecycleState::Active) {
            let bond_amt = fsm.ladder.initial_bond;
            let bond = Bond {
                currency: "USDC".into(),
                amount: bond_amt,
                bonded_party: "raas_oracle_authority".into(),
            };
            let propose_ts = eval_time.max(fsm.temporal_rules.cutoff_time);
            fsm.propose(report.outcome, bond, report.payload_commitment, propose_ts)
                .map_err(|e| e.to_string())?;

            let expires_at = propose_ts + fsm.temporal_rules.challenge_window_seconds;
            self.latest_commitments
                .insert(market_id.to_string(), report.payload_commitment);
            self.scheduler.complete_market(market_id);

            self.ledger.append(
                propose_ts,
                LedgerEvent::OutcomeProposed {
                    market_id: market_id.to_string(),
                    outcome_code: report.outcome,
                    proposed_at: propose_ts,
                    expires_at,
                    bond_amount: bond_amt,
                    payload_commitment: report.payload_commitment,
                },
            );
        }

        Ok((report, fsm.state.clone()))
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSummaryDto {
    pub market_id: String,
    pub question: String,
    pub origin_key: String,
    pub state: MarketLifecycleState,
    pub has_receipt: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PollLiveMarketResponse {
    pub market_id: String,
    pub attestation: AttestedPayload,
    pub report: VerifiedResolutionReport,
    pub fsm_state: MarketLifecycleState,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoverLiveFixtureRequest {
    pub sport_path: String,
    pub event_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoverLiveFixtureResponse {
    pub sport_path: String,
    pub discovered_events: Vec<DiscoveredEspnEvent>,
    pub selected_event_id: String,
    pub matrix: FixtureMarketMatrix,
    pub evaluations: Vec<(String, ConfirmationStepResult)>,
    pub attestation: AttestedPayload,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PollLiveFixtureRequest {
    pub sport_path: String,
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
        .route("/v1/markets", get(handle_list_markets))
        .route("/v1/markets/blueprint", post(handle_register_blueprint))
        .route(
            "/v1/markets/:market_id/poll-live",
            post(handle_poll_live_market),
        )
        .route(
            "/v1/sports/fixtures/matrix",
            post(handle_register_fixture_matrix),
        )
        .route(
            "/v1/sports/fixtures/discover-live",
            post(handle_discover_live_fixture),
        )
        .route(
            "/v1/sports/fixtures/:event_id/ingest",
            post(handle_ingest_fixture_snapshot),
        )
        .route(
            "/v1/sports/fixtures/:event_id/poll-live",
            post(handle_poll_live_fixture),
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

async fn handle_list_markets(
    State(state): State<Arc<RwLock<RaasEngine>>>,
) -> impl IntoResponse {
    let engine = state.read().await;
    let list = engine.list_markets();
    (StatusCode::OK, Json(list))
}

async fn handle_register_blueprint(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Json(req): Json<RegisterBlueprintRequest>,
) -> impl IntoResponse {
    let mut engine = state.write().await;
    let spec = engine.register_blueprint(req.market_id, req.blueprint, req.registered_at);
    (StatusCode::CREATED, Json(spec))
}

async fn handle_poll_live_market(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Path(market_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let (spec, provider) = {
        let engine = state.read().await;
        let spec = engine
            .general_specs
            .get(&market_id)
            .cloned()
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!("General market '{}' not found", market_id),
                )
            })?;
        (spec, engine.http_provider.clone())
    };

    let mut candidate_urls = vec![spec.primary_source_url.clone()];
    candidate_urls.extend(spec.secondary_source_urls.clone());

    let mut last_err = String::from("No oracle URLs configured");
    let mut fetched_attestation: Option<AttestedPayload> = None;

    for url in &candidate_urls {
        match provider.fetch_attested(url).await {
            Ok(att) => {
                fetched_attestation = Some(att);
                break;
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
    }

    let attestation = fetched_attestation.ok_or((StatusCode::BAD_GATEWAY, last_err))?;

    let mut engine = state.write().await;
    let (report, fsm_state) = engine
        .ingest_general_attestation(&market_id, &attestation)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    Ok((
        StatusCode::OK,
        Json(PollLiveMarketResponse {
            market_id,
            attestation,
            report,
            fsm_state,
        }),
    ))
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

async fn handle_discover_live_fixture(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Json(req): Json<DiscoverLiveFixtureRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let provider = {
        let engine = state.read().await;
        engine.http_provider.clone()
    };

    let discovered_events = provider
        .discover_live_espn_events(&req.sport_path)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let selected_event_id = if let Some(explicit_id) = req.event_id {
        explicit_id
    } else {
        discovered_events
            .iter()
            .find(|ev| ev.completed || ev.state == "in")
            .or_else(|| discovered_events.first())
            .map(|ev| ev.event_id.clone())
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    format!(
                        "No live or scheduled ESPN events found on scoreboard for '{}'",
                        req.sport_path
                    ),
                )
            })?
    };

    let attestation = provider
        .fetch_espn_summary_attested(&req.sport_path, &selected_event_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let raw_json = String::from_utf8_lossy(&attestation.raw_bytes).to_string();
    let mut engine = state.write().await;

    let matrix = engine
        .register_sports_fixture_matrix(
            &req.sport_path,
            &selected_event_id,
            &raw_json,
            attestation.fetch_timestamp,
        )
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let evaluations = engine
        .ingest_sports_snapshot(&req.sport_path, &selected_event_id, &attestation)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    Ok((
        StatusCode::CREATED,
        Json(DiscoverLiveFixtureResponse {
            sport_path: req.sport_path,
            discovered_events,
            selected_event_id,
            matrix,
            evaluations,
            attestation,
        }),
    ))
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

async fn handle_poll_live_fixture(
    State(state): State<Arc<RwLock<RaasEngine>>>,
    Path(event_id): Path<String>,
    Json(req): Json<PollLiveFixtureRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let provider = {
        let engine = state.read().await;
        engine.http_provider.clone()
    };

    let attestation = provider
        .fetch_espn_summary_attested(&req.sport_path, &event_id)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;

    let mut engine = state.write().await;
    let evaluations = engine
        .ingest_sports_snapshot(&req.sport_path, &event_id, &attestation)
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

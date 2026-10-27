use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use rand::rngs::OsRng;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower::ServiceExt;

use resolution_service::fsm::MarketLifecycleState;
use resolution_service::ledger::{AuditLedger, LedgerError, LedgerEvent};
use resolution_service::provenance::mock::MockAttestationAuthority;
use resolution_service::receipt::{
    verify_onchain_receipt, ReceiptError, SettlementReceipt, RECEIPT_WIRE_LEN,
};
use resolution_service::scheduler::ConfirmationStepResult;
use resolution_service::server::{
    build_router, FinalizeMarketRequest, IngestFixtureSnapshotRequest,
    IngestFixtureSnapshotResponse, LedgerAuditResponse, RaasEngine, RegisterFixtureMatrixRequest,
};
use resolution_service::sports::FixtureMarketMatrix;

/// TEST 1: Multi-Snapshot VAR Rollback vs. Confirmation Depth
/// Proves that an in-play goal at t=1700000900 is held as TentativeObserved (not immediately Proposed),
/// is cleanly reverted if VAR rolls the score back to 0-0 at t=1700000990,
/// and only transitions to Proposed after persisting >= 180s across consecutive attested snapshots.
#[test]
fn test_multi_snapshot_var_rollback_vs_confirmation_depth() {
    let authority = MockAttestationAuthority::new();
    let mut engine = RaasEngine::new(180); // 180s VAR confirmation depth

    let pre_match_json = r#"{
        "header": {
            "id": "777",
            "competitions": [{
                "id": "777",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 1,
                    "displayClock": "0'",
                    "type": { "name": "STATUS_SCHEDULED", "state": "pre", "completed": false }
                },
                "competitors": [
                    { "id": "1", "homeAway": "home", "score": "0", "team": { "displayName": "Arsenal" } },
                    { "id": "2", "homeAway": "away", "score": "0", "team": { "displayName": "Chelsea" } }
                ]
            }]
        }
    }"#;

    engine
        .register_sports_fixture_matrix("soccer/eng.1", "777", pre_match_json, 1_700_000_000)
        .unwrap();

    // Snapshot 1 (t = 1_700_000_900, 15th minute): Arsenal scores 1-0 (no VAR string in feed yet)
    let snap1_goal_json = br#"{
        "header": {
            "id": "777",
            "competitions": [{
                "id": "777",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 1,
                    "displayClock": "15'",
                    "type": { "name": "STATUS_IN_PROGRESS", "state": "in", "completed": false }
                },
                "competitors": [
                    { "id": "1", "homeAway": "home", "score": "1", "team": { "displayName": "Arsenal" } },
                    { "id": "2", "homeAway": "away", "score": "0", "team": { "displayName": "Chelsea" } }
                ]
            }]
        },
        "plays": [{
            "id": "g1",
            "scoringPlay": true,
            "period": { "number": 1 },
            "clock": { "displayValue": "15'", "value": 900.0 },
            "type": { "text": "Goal" },
            "text": "Goal! Arsenal 1, Chelsea 0.",
            "team": { "id": "1" }
        }]
    }"#;

    let att1 = authority.create_attested_payload(
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=777",
        snap1_goal_json,
        1_700_000_900,
        vec![],
    );

    let evals1 = engine
        .ingest_sports_snapshot("soccer/eng.1", "777", &att1)
        .unwrap();
    let first_20m_step1 = &evals1
        .iter()
        .find(|(m, _)| m == "777-first-20m-goal")
        .unwrap()
        .1;

    assert!(
        matches!(
            first_20m_step1,
            ConfirmationStepResult::TentativeObserved {
                outcome_code: 1,
                ..
            }
        ),
        "First observation of in-play goal must be held as TentativeObserved"
    );
    assert_eq!(
        engine.fsms.get("777-first-20m-goal").unwrap().state,
        MarketLifecycleState::Active,
        "FSM must remain Active during VAR confirmation window"
    );

    // Snapshot 2 (t = 1_700_000_990, 90s later): VAR overturns the goal! Score reverts to 0-0.
    let snap2_rollback_json = br#"{
        "header": {
            "id": "777",
            "competitions": [{
                "id": "777",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 1,
                    "displayClock": "16'",
                    "type": { "name": "STATUS_IN_PROGRESS", "state": "in", "completed": false }
                },
                "competitors": [
                    { "id": "1", "homeAway": "home", "score": "0", "team": { "displayName": "Arsenal" } },
                    { "id": "2", "homeAway": "away", "score": "0", "team": { "displayName": "Chelsea" } }
                ]
            }]
        },
        "plays": []
    }"#;

    let att2 = authority.create_attested_payload(
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=777",
        snap2_rollback_json,
        1_700_000_990,
        vec![],
    );

    let evals2 = engine
        .ingest_sports_snapshot("soccer/eng.1", "777", &att2)
        .unwrap();
    let first_20m_step2 = &evals2
        .iter()
        .find(|(m, _)| m == "777-first-20m-goal")
        .unwrap()
        .1;

    assert!(
        matches!(
            first_20m_step2,
            ConfirmationStepResult::CandidateReverted {
                reverted_outcome_code: 1,
                ..
            }
        ),
        "VAR rollback must revert tentative candidate without proposing outcome"
    );
    assert!(engine
        .confirmation_tracker
        .get_candidate("777-first-20m-goal")
        .is_none());

    // Snapshot 3 (t = 1_700_001_050, 18th minute): Legitimate goal scored (1-0)
    let att3 = authority.create_attested_payload(
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=777",
        snap1_goal_json,
        1_700_001_050,
        vec![],
    );
    engine
        .ingest_sports_snapshot("soccer/eng.1", "777", &att3)
        .unwrap();
    assert_eq!(
        engine.fsms.get("777-first-20m-goal").unwrap().state,
        MarketLifecycleState::Active
    );

    // Snapshot 4 (t = 1_700_001_240, 190s after Snapshot 3 >= 180s depth): Goal still stands!
    let att4 = authority.create_attested_payload(
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=777",
        snap1_goal_json,
        1_700_001_240,
        vec![],
    );
    let evals4 = engine
        .ingest_sports_snapshot("soccer/eng.1", "777", &att4)
        .unwrap();
    let first_20m_step4 = &evals4
        .iter()
        .find(|(m, _)| m == "777-first-20m-goal")
        .unwrap()
        .1;

    assert!(
        matches!(
            first_20m_step4,
            ConfirmationStepResult::Confirmed {
                outcome_code: 1,
                ..
            }
        ),
        "Goal surviving >= 180s confirmation depth must be promoted to Confirmed"
    );
    assert!(
        matches!(
            engine.fsms.get("777-first-20m-goal").unwrap().state,
            MarketLifecycleState::Proposed { outcome: 1, .. }
        ),
        "FSM must transition to Proposed(1) once confirmation depth is satisfied"
    );

    // Verify ledger recorded the full sequence including TentativeOutcomeReverted
    assert!(engine.ledger.verify_chain_integrity().is_ok());
}

/// TEST 2: Hash-Chained Audit Ledger Integrity & Tamper Detection
#[test]
fn test_hash_chained_audit_ledger_integrity_and_tamper_rejection() {
    let mut ledger = AuditLedger::new();

    ledger.append(
        1000,
        LedgerEvent::MarketRegistered {
            market_id: "m1".into(),
            origin_key: "soccer/eng.1:1".into(),
            question: "Will Arsenal win?".into(),
        },
    );
    ledger.append(
        1100,
        LedgerEvent::TentativeOutcomeObserved {
            market_id: "m1".into(),
            outcome_code: 1,
            first_seen_at: 1100,
            payload_commitment: [1u8; 32],
        },
    );
    ledger.append(
        1300,
        LedgerEvent::OutcomeProposed {
            market_id: "m1".into(),
            outcome_code: 1,
            proposed_at: 1300,
            expires_at: 3100,
            bond_amount: Decimal::from(100),
            payload_commitment: [2u8; 32],
        },
    );

    assert!(ledger.verify_chain_integrity().is_ok());

    // 1. Mutate an historical event payload -> EntryHashTampered
    let mut tampered_payload = ledger.clone();
    tampered_payload.entries[1].event = LedgerEvent::TentativeOutcomeObserved {
        market_id: "m1".into(),
        outcome_code: 2, // flipped 1 -> 2
        first_seen_at: 1100,
        payload_commitment: [1u8; 32],
    };
    assert!(matches!(
        tampered_payload.verify_chain_integrity(),
        Err(LedgerError::EntryHashTampered { seq_no: 1, .. })
    ));

    // 2. Delete an entry from the middle -> SequenceMismatch
    let mut deleted_entry = ledger.clone();
    deleted_entry.entries.remove(1);
    assert!(matches!(
        deleted_entry.verify_chain_integrity(),
        Err(LedgerError::SequenceMismatch { index: 1, .. })
    ));
}

/// TEST 3: Binary-Deterministic 153-Byte On-Chain Settlement Receipts
#[test]
fn test_binary_onchain_settlement_receipt_verification() {
    let mut csprng = OsRng;
    let signing_key = SigningKey::generate(&mut csprng);

    // 1. YES Receipt ([10_000, 0])
    let yes_receipt = SettlementReceipt::issue(
        &signing_key,
        "btc-70k",
        1,
        [11u8; 32],
        [22u8; 32],
        1_700_005_000,
    )
    .unwrap();
    assert_eq!(yes_receipt.to_wire_bytes().len(), RECEIPT_WIRE_LEN);
    assert_eq!(yes_receipt.payout_numerators, [10_000, 0]);
    assert!(verify_onchain_receipt(&yes_receipt).is_ok());

    // 2. NO Receipt ([0, 10_000])
    let no_receipt = SettlementReceipt::issue(
        &signing_key,
        "btc-70k",
        2,
        [11u8; 32],
        [22u8; 32],
        1_700_005_000,
    )
    .unwrap();
    assert_eq!(no_receipt.payout_numerators, [0, 10_000]);
    assert!(verify_onchain_receipt(&no_receipt).is_ok());

    // 3. VOID Receipt ([5_000, 5_000] 50-50 refund)
    let void_receipt = SettlementReceipt::issue(
        &signing_key,
        "salah-anytime-dnp",
        3,
        [11u8; 32],
        [22u8; 32],
        1_700_005_000,
    )
    .unwrap();
    assert_eq!(void_receipt.payout_numerators, [5_000, 5_000]);
    assert!(verify_onchain_receipt(&void_receipt).is_ok());

    // 4. Tamper with payout numerators -> InvalidPayoutVector
    let mut bad_payout = yes_receipt.clone();
    bad_payout.payout_numerators = [9_000, 1_000];
    assert!(matches!(
        verify_onchain_receipt(&bad_payout),
        Err(ReceiptError::InvalidPayoutVector { .. })
    ));

    // 5. Tamper with finalized_at timestamp -> InvalidSignature
    let mut bad_sig = yes_receipt.clone();
    bad_sig.finalized_at += 1;
    assert!(matches!(
        verify_onchain_receipt(&bad_sig),
        Err(ReceiptError::InvalidSignature(_))
    ));
}

/// TEST 4: End-to-End HTTP Service Router Integration
#[tokio::test]
async fn test_end_to_end_http_service_router_workflow() {
    let state = Arc::new(RwLock::new(RaasEngine::new(180)));
    let app = build_router(state.clone());

    let fixture_json = r#"{
        "header": {
            "id": "401696",
            "competitions": [{
                "id": "401696",
                "date": "2026-10-27T19:00:00Z",
                "status": {
                    "period": 2,
                    "displayClock": "90'+4'",
                    "type": { "name": "STATUS_FULL_TIME", "state": "post", "completed": true }
                },
                "competitors": [
                    {
                        "id": "1",
                        "homeAway": "home",
                        "score": "2",
                        "team": { "displayName": "Arsenal" },
                        "linescores": [{ "displayValue": "1" }, { "displayValue": "1" }]
                    },
                    {
                        "id": "2",
                        "homeAway": "away",
                        "score": "1",
                        "team": { "displayName": "Chelsea" },
                        "linescores": [{ "displayValue": "0" }, { "displayValue": "1" }]
                    }
                ]
            }]
        },
        "plays": [
            {
                "id": "g1",
                "scoringPlay": true,
                "period": { "number": 1 },
                "clock": { "displayValue": "14'", "value": 840.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Bukayo Saka scores.",
                "team": { "id": "1" },
                "participants": [{ "athlete": { "id": "77", "displayName": "Bukayo Saka" } }]
            },
            {
                "id": "g2",
                "scoringPlay": true,
                "period": { "number": 2 },
                "clock": { "displayValue": "55'", "value": 3300.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Cole Palmer scores.",
                "team": { "id": "2" },
                "participants": [{ "athlete": { "id": "88", "displayName": "Cole Palmer" } }]
            },
            {
                "id": "g3",
                "scoringPlay": true,
                "period": { "number": 2 },
                "clock": { "displayValue": "82'", "value": 4920.0 },
                "type": { "text": "Goal" },
                "text": "Goal! Bukayo Saka scores again.",
                "team": { "id": "1" },
                "participants": [{ "athlete": { "id": "77", "displayName": "Bukayo Saka" } }]
            }
        ],
        "rosters": [{
            "team": { "id": "1", "displayName": "Arsenal" },
            "roster": [
                {
                    "starter": true,
                    "subbedIn": false,
                    "athlete": { "id": "77", "displayName": "Bukayo Saka" },
                    "position": { "name": "Forward" }
                },
                {
                    "starter": false,
                    "subbedIn": false,
                    "athlete": { "id": "99", "displayName": "Raheem Sterling" },
                    "position": { "name": "Forward" }
                }
            ]
        }]
    }"#;

    // 1. POST /v1/sports/fixtures/matrix -> Register entire 10-market derivative matrix
    let reg_req = RegisterFixtureMatrixRequest {
        sport_path: "soccer/eng.1".into(),
        event_id: "401696".into(),
        raw_fixture_json: fixture_json.into(),
        registered_at: 1_700_000_000,
    };

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sports/fixtures/matrix")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&reg_req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let matrix: FixtureMarketMatrix = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(matrix.templates.len(), 10);

    // 2. POST /v1/sports/fixtures/401696/ingest -> Submit attested full-time snapshot
    let authority = MockAttestationAuthority::new();
    let attestation = authority.create_attested_payload(
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=401696",
        fixture_json.as_bytes(),
        1_800_000_000,
        vec![("content-type".into(), "application/json".into())],
    );

    let ingest_req = IngestFixtureSnapshotRequest {
        sport_path: "soccer/eng.1".into(),
        attestation,
    };

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sports/fixtures/401696/ingest")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&ingest_req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let ingest_resp: IngestFixtureSnapshotResponse = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(ingest_resp.evaluations.len(), 10);

    // 3. GET /v1/markets/401696-anytime-goal-raheem-sterling/receipt -> DNP player already has VOID receipt
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/markets/401696-anytime-goal-raheem-sterling/receipt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let void_receipt: SettlementReceipt = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(void_receipt.final_outcome, 3);
    assert_eq!(void_receipt.payout_numerators, [5_000, 5_000]);
    assert!(verify_onchain_receipt(&void_receipt).is_ok());

    // 4. POST /v1/markets/401696-arsenal-home-win/finalize -> Finalize past challenge window
    let fin_req = FinalizeMarketRequest {
        current_time: 1_800_005_000,
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/markets/401696-arsenal-home-win/finalize")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&fin_req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let win_receipt: SettlementReceipt = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(win_receipt.final_outcome, 1);
    assert_eq!(win_receipt.payout_numerators, [10_000, 0]);
    assert!(verify_onchain_receipt(&win_receipt).is_ok());

    // 5. GET /v1/ledger/audit -> Verify full hash-chained ledger integrity
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/ledger/audit")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let audit: LedgerAuditResponse = serde_json::from_slice(&body_bytes).unwrap();
    assert!(audit.valid);
    assert_eq!(audit.entry_count, 21); // 10 registrations + 8 proposals + 2 voids + 1 finalization
}

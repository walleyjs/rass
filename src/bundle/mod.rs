use chrono::{TimeZone, Utc};
use ed25519_dalek::{Signer, SigningKey};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::str::FromStr;

use crate::ast::ComparisonOperator;
use crate::blueprints::{CompiledMarketSpec, MarketBlueprint};
use crate::fsm::{Bond, EscalationLadder, ResolutionFsm};
use crate::ledger::{AuditLedger, LedgerEvent};
use crate::provenance::{AttestationEnvelope, AttestedPayload};
use crate::receipt::{verify_onchain_receipt, SettlementReceipt};
use crate::sports::{
    FixtureMarketMatrix, MultiplexedFixtureResolutionReport, MultiplexedSportsResolver,
    NormalizedFixtureState, SportsMarketTemplate,
};
use crate::verifier::{verify_resolution_offline, VerifiedResolutionReport};

pub const GOLDEN_SEED: [u8; 32] = [0x42; 32];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralCanaryEntry {
    pub market_id: String,
    pub spec: CompiledMarketSpec,
    pub attestation: AttestedPayload,
    pub report: VerifiedResolutionReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SportsCanaryEntry {
    pub sport_path: String,
    pub event_id: String,
    pub fixture_title: String,
    pub templates: Vec<SportsMarketTemplate>,
    pub attestation: AttestedPayload,
    pub report: MultiplexedFixtureResolutionReport,
}

/// Self-contained, air-gapped verification bundle capturing signed HTTP wire payloads,
/// Ed25519 commitments, AST/Sports evaluation reports, 153-byte on-chain receipts,
/// and the hash-chained AuditLedger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveCanaryBundle {
    pub generated_at: i64,
    pub oracle_pubkey_hex: String,
    pub general_markets: Vec<GeneralCanaryEntry>,
    pub sports_fixture: Option<SportsCanaryEntry>,
    pub receipts: Vec<SettlementReceipt>,
    pub ledger: AuditLedger,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestationGoldenVector {
    pub source_url: String,
    pub fetch_timestamp: i64,
    pub raw_body_utf8: String,
    pub payload_sha256_hex: String,
    pub commitment_hash_hex: String,
    pub signature_hex: String,
    pub attested_payload: AttestedPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptGoldenVector {
    pub label: String,
    pub market_id: String,
    pub market_id_hash_hex: String,
    pub final_outcome: u8,
    pub payout_numerators: [u64; 2],
    pub payout_denominator: u64,
    pub attestation_commitment_hex: String,
    pub ledger_head_hash_hex: String,
    pub finalized_at: i64,
    pub wire_bytes_hex: String,
    pub receipt_hash_hex: String,
    pub oracle_signature_hex: String,
    pub receipt: SettlementReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerGoldenVector {
    pub genesis_hash_hex: String,
    pub head_hash_hex: String,
    pub ledger: AuditLedger,
}

/// Canonical cross-language golden test vector suite generated from deterministic seed `[0x42; 32]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenTestVectorsV1 {
    pub version: String,
    pub seed_hex: String,
    pub oracle_pubkey_hex: String,
    pub attestation_vector: AttestationGoldenVector,
    pub receipt_vectors: Vec<ReceiptGoldenVector>,
    pub ledger_vector: LedgerGoldenVector,
}

/// Pure, 100% offline verification of a `LiveCanaryBundle`.
pub fn verify_canary_bundle(bundle: &LiveCanaryBundle) -> Result<(), String> {
    // 1. Verify General Markets
    for entry in &bundle.general_markets {
        entry
            .attestation
            .verify_integrity()
            .map_err(|e| format!("[{}] Provenance failure: {}", entry.market_id, e))?;

        let eval_ts = entry
            .attestation
            .fetch_timestamp
            .max(entry.spec.temporal_rules.cutoff_time);
        let recomputed =
            verify_resolution_offline(&entry.market_id, &entry.spec, &entry.attestation, eval_ts)
                .map_err(|e| {
                format!(
                    "[{}] Offline AST verification failure: {}",
                    entry.market_id, e
                )
            })?;

        if recomputed.outcome != entry.report.outcome
            || recomputed.payload_commitment != entry.report.payload_commitment
            || recomputed.evaluated_value != entry.report.evaluated_value
        {
            return Err(format!(
                "[{}] Recomputed report mismatch: expected {:?}, got {:?}",
                entry.market_id, entry.report, recomputed
            ));
        }
    }

    // 2. Verify Multiplexed Sports Fixture Matrix
    if let Some(sports) = &bundle.sports_fixture {
        let resolver = MultiplexedSportsResolver::default();
        resolver
            .verify_report_offline(&sports.attestation, &sports.templates, &sports.report)
            .map_err(|e| {
                format!(
                    "[Sports {}] Offline verification failed: {}",
                    sports.event_id, e
                )
            })?;
    }

    // 3. Verify Append-Only Hash-Chained AuditLedger
    bundle
        .ledger
        .verify_chain_integrity()
        .map_err(|e| format!("Ledger chain integrity broken: {}", e))?;

    // 4. Verify 153-Byte On-Chain Settlement Receipts
    for receipt in &bundle.receipts {
        verify_onchain_receipt(receipt).map_err(|e| {
            format!(
                "[Receipt {}] Invalid on-chain receipt: {}",
                receipt.market_id, e
            )
        })?;
    }

    Ok(())
}

pub fn create_signed_payload_with_key(
    signing_key: &SigningKey,
    url: &str,
    raw_bytes: &[u8],
    fetch_timestamp: i64,
    headers: Vec<(String, String)>,
) -> AttestedPayload {
    let mut hasher = Sha256::new();
    hasher.update(raw_bytes);
    let payload_sha256: [u8; 32] = hasher.finalize().into();

    let mut canonical_headers: Vec<(String, String)> = headers
        .into_iter()
        .map(|(k, v)| (k.to_lowercase(), v))
        .collect();
    canonical_headers.sort_by(|a, b| a.0.cmp(&b.0));

    let pubkey = signing_key.verifying_key().to_bytes();

    let mut payload = AttestedPayload {
        raw_bytes: raw_bytes.to_vec(),
        payload_sha256,
        canonical_headers,
        source_url: url.to_string(),
        fetch_timestamp,
        rfc3161_timestamp_token: None,
        server_tls_cert_sha256: None,
        attestation: AttestationEnvelope::Ed25519Authority {
            authority_pubkey: pubkey,
            signature: Vec::new(),
        },
    };

    let commitment = payload.commitment_hash();
    let signature = signing_key.sign(&commitment);
    payload.attestation = AttestationEnvelope::Ed25519Authority {
        authority_pubkey: pubkey,
        signature: signature.to_bytes().to_vec(),
    };

    payload
}

/// Builds the canonical `GoldenTestVectorsV1` from `GOLDEN_SEED` (`[0x42; 32]`).
pub fn build_golden_vectors_v1() -> GoldenTestVectorsV1 {
    let signing_key = SigningKey::from_bytes(&GOLDEN_SEED);
    let pubkey = signing_key.verifying_key().to_bytes();

    let url = "https://api.binance.com/api/v3/ticker/price?symbol=BTCUSDT";
    let raw_body = r#"{"symbol":"BTCUSDT","price":"71250.50"}"#;
    let fetch_ts = 1_700_000_000_i64;

    let attested = create_signed_payload_with_key(
        &signing_key,
        url,
        raw_body.as_bytes(),
        fetch_ts,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            (
                "date".to_string(),
                "Tue, 14 Nov 2023 22:13:20 GMT".to_string(),
            ),
            (
                "etag".to_string(),
                "\"d41d8cd98f00b204e9800998ecf8427e\"".to_string(),
            ),
        ],
    );

    let commitment = attested.commitment_hash();
    let sig_hex = match &attested.attestation {
        AttestationEnvelope::Ed25519Authority { signature, .. } => hex::encode(signature),
        _ => String::new(),
    };

    let attestation_vector = AttestationGoldenVector {
        source_url: url.to_string(),
        fetch_timestamp: fetch_ts,
        raw_body_utf8: raw_body.to_string(),
        payload_sha256_hex: hex::encode(attested.payload_sha256),
        commitment_hash_hex: hex::encode(commitment),
        signature_hex: sig_hex,
        attested_payload: attested,
    };

    let mut ledger = AuditLedger::new();
    ledger.append(
        1_700_000_000,
        LedgerEvent::MarketRegistered {
            market_id: "btc-70k-golden".to_string(),
            origin_key: url.to_string(),
            question: "Will BTC (BTCUSDT) exceed $70000 at 2023-11-14 22:13 UTC?".to_string(),
        },
    );
    ledger.append(
        1_700_000_000,
        LedgerEvent::OutcomeProposed {
            market_id: "btc-70k-golden".to_string(),
            outcome_code: 1,
            proposed_at: 1_700_000_000,
            expires_at: 1_700_001_800,
            bond_amount: Decimal::from(100),
            payload_commitment: commitment,
        },
    );
    let ledger_head_before_finalize = ledger.head_hash();

    let finalized_at = 1_700_001_801_i64;
    let cases = [
        ("YES_OUTCOME_1", "btc-70k-golden", 1_u8),
        ("NO_OUTCOME_2", "btc-80k-golden", 2_u8),
        ("VOID_OUTCOME_3", "player-prop-dnp-golden", 3_u8),
    ];

    let mut receipt_vectors = Vec::with_capacity(cases.len());
    for (label, mid, outcome) in cases {
        let receipt = SettlementReceipt::issue(
            &signing_key,
            mid,
            outcome,
            commitment,
            ledger_head_before_finalize,
            finalized_at,
        )
        .expect("issue golden receipt");

        let wire = receipt.to_wire_bytes();
        let r_hash = receipt.receipt_hash();

        if mid == "btc-70k-golden" {
            ledger.append(
                finalized_at,
                LedgerEvent::MarketFinalized {
                    market_id: mid.to_string(),
                    final_outcome: outcome,
                    finalized_at,
                    receipt_hash: r_hash,
                },
            );
        }

        receipt_vectors.push(ReceiptGoldenVector {
            label: label.to_string(),
            market_id: mid.to_string(),
            market_id_hash_hex: hex::encode(receipt.market_id_hash),
            final_outcome: outcome,
            payout_numerators: receipt.payout_numerators,
            payout_denominator: receipt.payout_denominator,
            attestation_commitment_hex: hex::encode(receipt.attestation_commitment),
            ledger_head_hash_hex: hex::encode(receipt.ledger_head_hash),
            finalized_at,
            wire_bytes_hex: hex::encode(wire),
            receipt_hash_hex: hex::encode(r_hash),
            oracle_signature_hex: hex::encode(&receipt.oracle_signature),
            receipt,
        });
    }

    let ledger_vector = LedgerGoldenVector {
        genesis_hash_hex: hex::encode(AuditLedger::GENESIS_HASH),
        head_hash_hex: hex::encode(ledger.head_hash()),
        ledger,
    };

    GoldenTestVectorsV1 {
        version: "RAAS_GOLDEN_VECTORS_V1".to_string(),
        seed_hex: hex::encode(GOLDEN_SEED),
        oracle_pubkey_hex: hex::encode(pubkey),
        attestation_vector,
        receipt_vectors,
        ledger_vector,
    }
}

/// Builds a deterministic, air-gapped `LiveCanaryBundle` using `GOLDEN_SEED` (`[0x42; 32]`)
/// for committing to `vectors/sample_canary_bundle.json`.
pub fn build_deterministic_sample_canary_bundle() -> LiveCanaryBundle {
    let signing_key = SigningKey::from_bytes(&GOLDEN_SEED);
    let pubkey_hex = hex::encode(signing_key.verifying_key().to_bytes());
    let cutoff = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let fetch_ts = 1_700_000_060_i64;

    let mut ledger = AuditLedger::new();
    let mut general_markets = Vec::new();
    let mut receipts = Vec::new();

    let general_defs = vec![
        (
            "btc-usdt-spot-70k",
            MarketBlueprint::CryptoSpotCross {
                symbol: "BTCUSDT".to_string(),
                strike_price: Decimal::from(70000),
                operator: ComparisonOperator::GreaterThan,
                cutoff_time: cutoff,
            },
            r#"{"symbol":"BTCUSDT","price":"71250.50"}"#,
        ),
        (
            "london-temp-15c",
            MarketBlueprint::WeatherThreshold {
                location: "London".to_string(),
                latitude: Decimal::from_str("51.5074").unwrap(),
                longitude: Decimal::from_str("-0.1278").unwrap(),
                threshold_celsius: Decimal::from(15),
                operator: ComparisonOperator::LessThan,
                cutoff_time: cutoff,
            },
            r#"{"current":{"temperature_2m":11.4}}"#,
        ),
        (
            "rust-release-1-75",
            MarketBlueprint::SoftwareRelease {
                owner: "rust-lang".to_string(),
                repo: "rust".to_string(),
                target_version: "1.75.0".to_string(),
                operator: ComparisonOperator::GreaterThanOrEqual,
                cutoff_time: cutoff,
            },
            r#"{"tag_name":"1.82.0"}"#,
        ),
    ];

    for (mid, bp, raw_json) in general_defs {
        let spec = bp.compile();
        ledger.append(
            cutoff.timestamp(),
            LedgerEvent::MarketRegistered {
                market_id: mid.to_string(),
                origin_key: spec.primary_source_url.clone(),
                question: spec.question.clone(),
            },
        );

        let attestation = create_signed_payload_with_key(
            &signing_key,
            &spec.primary_source_url,
            raw_json.as_bytes(),
            fetch_ts,
            vec![("content-type".to_string(), "application/json".to_string())],
        );

        let mut report =
            verify_resolution_offline(mid, &spec, &attestation, fetch_ts).expect("valid report");
        // Pin verified_at to fetch_ts for 100% deterministic JSON serialization
        report.verified_at = fetch_ts;

        let mut fsm = ResolutionFsm::new(
            mid.to_string(),
            spec.temporal_rules.clone(),
            EscalationLadder::default(),
        );
        let bond_amt = fsm.ladder.initial_bond;
        fsm.propose(
            report.outcome,
            Bond {
                currency: "USDC".to_string(),
                amount: bond_amt,
                bonded_party: "raas_oracle_authority".to_string(),
            },
            report.payload_commitment,
            fetch_ts,
        )
        .expect("propose");

        let expires_at = fetch_ts + spec.temporal_rules.challenge_window_seconds;
        ledger.append(
            fetch_ts,
            LedgerEvent::OutcomeProposed {
                market_id: mid.to_string(),
                outcome_code: report.outcome,
                proposed_at: fetch_ts,
                expires_at,
                bond_amount: bond_amt,
                payload_commitment: report.payload_commitment,
            },
        );

        let finalize_ts = expires_at + 1;
        let final_outcome = fsm.finalize(finalize_ts).expect("finalize");
        let receipt = SettlementReceipt::issue(
            &signing_key,
            mid,
            final_outcome,
            report.payload_commitment,
            ledger.head_hash(),
            finalize_ts,
        )
        .expect("receipt");

        ledger.append(
            finalize_ts,
            LedgerEvent::MarketFinalized {
                market_id: mid.to_string(),
                final_outcome,
                finalized_at: finalize_ts,
                receipt_hash: receipt.receipt_hash(),
            },
        );

        receipts.push(receipt);
        general_markets.push(GeneralCanaryEntry {
            market_id: mid.to_string(),
            spec,
            attestation,
            report,
        });
    }

    // Deterministic Sports Fixture Matrix
    let espn_summary_json = r#"{
        "header": {
            "id": "401696",
            "competitions": [{
                "id": "401696",
                "date": "2023-11-14T19:00:00Z",
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

    let fixture =
        NormalizedFixtureState::from_espn_summary_json("soccer/eng.1", "401696", espn_summary_json)
            .expect("valid fixture");
    let matrix = FixtureMarketMatrix::generate_from_fixture(&fixture);

    for tmpl in &matrix.templates {
        ledger.append(
            cutoff.timestamp(),
            LedgerEvent::MarketRegistered {
                market_id: tmpl.market_id.clone(),
                origin_key: "soccer/eng.1:401696".to_string(),
                question: tmpl.question.clone(),
            },
        );
    }

    let sports_attestation = create_signed_payload_with_key(
        &signing_key,
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=401696",
        espn_summary_json.as_bytes(),
        fetch_ts,
        vec![("content-type".to_string(), "application/json".to_string())],
    );

    let resolver = MultiplexedSportsResolver::default();
    let sports_report = resolver
        .resolve_matrix(
            "soccer/eng.1",
            "401696",
            &sports_attestation,
            &matrix.templates,
        )
        .expect("resolve sports matrix");

    for verdict in &sports_report.verdicts {
        let receipt = SettlementReceipt::issue(
            &signing_key,
            &verdict.market_id,
            verdict.outcome_code,
            sports_report.payload_commitment,
            ledger.head_hash(),
            fetch_ts + 3600,
        )
        .expect("sports receipt");

        if verdict.outcome_code == 3 {
            ledger.append(
                fetch_ts + 3600,
                LedgerEvent::MarketVoided {
                    market_id: verdict.market_id.clone(),
                    void_code: 3,
                    reason: "DidNotPlay void".to_string(),
                    voided_at: fetch_ts + 3600,
                    receipt_hash: receipt.receipt_hash(),
                },
            );
        } else {
            ledger.append(
                fetch_ts + 3600,
                LedgerEvent::MarketFinalized {
                    market_id: verdict.market_id.clone(),
                    final_outcome: verdict.outcome_code,
                    finalized_at: fetch_ts + 3600,
                    receipt_hash: receipt.receipt_hash(),
                },
            );
        }
        receipts.push(receipt);
    }

    receipts.sort_by(|a, b| a.market_id.cmp(&b.market_id));

    LiveCanaryBundle {
        generated_at: fetch_ts,
        oracle_pubkey_hex: pubkey_hex,
        general_markets,
        sports_fixture: Some(SportsCanaryEntry {
            sport_path: "soccer/eng.1".to_string(),
            event_id: "401696".to_string(),
            fixture_title: matrix.fixture_title,
            templates: matrix.templates,
            attestation: sports_attestation,
            report: sports_report,
        }),
        receipts,
        ledger,
    }
}

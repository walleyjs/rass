use chrono::{Duration as ChronoDuration, Utc};
use rust_decimal::Decimal;
use std::env;
use std::fs;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

use resolution_service::ast::ComparisonOperator;
use resolution_service::blueprints::MarketBlueprint;
use resolution_service::bundle::{
    build_deterministic_sample_canary_bundle, build_golden_vectors_v1, verify_canary_bundle,
    GeneralCanaryEntry, LiveCanaryBundle, SportsCanaryEntry,
};
use resolution_service::fsm::MarketLifecycleState;
use resolution_service::provenance::mock::MockAttestationAuthority;
use resolution_service::provenance::{AttestedPayload, AttestedPayloadProvider};
use resolution_service::receipt::SettlementReceipt;
use resolution_service::server::{build_router, RaasEngine};
use resolution_service::sports::{
    FixtureMarketMatrix, MultiplexedSportsResolver, NormalizedFixtureState,
};
use resolution_service::verifier::verify_resolution_offline;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let subcommand = args.get(1).map(|s| s.as_str()).unwrap_or("demo");

    match subcommand {
        "generate-vectors" => {
            let dir = args.get(2).map(|s| s.as_str()).unwrap_or("vectors");
            if let Err(e) = run_generate_vectors(dir) {
                eprintln!("Failed to generate golden vectors: {}", e);
                std::process::exit(1);
            }
        }
        "live-canary" => {
            let mut out_path = String::from("/tmp/live_canary.json");
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--out" && i + 1 < args.len() {
                    out_path = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if let Err(e) = run_live_canary(&out_path).await {
                eprintln!("Live canary failed: {}", e);
                std::process::exit(1);
            }
        }
        "verify-bundle" => {
            let path = args
                .get(2)
                .map(|s| s.as_str())
                .unwrap_or("/tmp/live_canary.json");
            if let Err(e) = run_verify_bundle(path) {
                eprintln!("Bundle verification FAILED: {}", e);
                std::process::exit(1);
            }
        }
        "serve" => {
            let mut port: u16 = 8080;
            let mut seed_live = false;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--port" && i + 1 < args.len() {
                    port = args[i + 1].parse().unwrap_or(8080);
                    i += 2;
                } else if args[i] == "--seed-live" {
                    seed_live = true;
                    i += 1;
                } else {
                    i += 1;
                }
            }
            if let Err(e) = run_server(port, seed_live).await {
                eprintln!("Server error: {}", e);
                std::process::exit(1);
            }
        }
        _ => {
            run_offline_demo();
        }
    }
}

async fn seed_engine_with_live_events(
    engine: &mut RaasEngine,
) -> Result<(Vec<GeneralCanaryEntry>, Option<SportsCanaryEntry>), String> {
    let now = Utc::now();
    // Set cutoff 60 seconds in the past so live fetches satisfy `fetch_timestamp >= cutoff_time`
    let cutoff = now - ChronoDuration::seconds(60);
    let registered_at = cutoff.timestamp();

    let blueprints = vec![
        (
            "btc-usdt-spot-live".to_string(),
            MarketBlueprint::CryptoSpotCross {
                symbol: "BTCUSDT".to_string(),
                strike_price: Decimal::from(50000),
                operator: ComparisonOperator::GreaterThan,
                cutoff_time: cutoff,
            },
        ),
        (
            "london-temp-live".to_string(),
            MarketBlueprint::WeatherThreshold {
                location: "London".to_string(),
                latitude: Decimal::from_str("51.5074").unwrap(),
                longitude: Decimal::from_str("-0.1278").unwrap(),
                threshold_celsius: Decimal::from(-10),
                operator: ComparisonOperator::GreaterThan,
                cutoff_time: cutoff,
            },
        ),
        (
            "rust-release-semver-live".to_string(),
            MarketBlueprint::SoftwareRelease {
                owner: "rust-lang".to_string(),
                repo: "rust".to_string(),
                target_version: "1.70.0".to_string(),
                operator: ComparisonOperator::GreaterThanOrEqual,
                cutoff_time: cutoff,
            },
        ),
    ];

    let provider = engine.http_provider.clone();
    let mut general_entries = Vec::new();

    for (market_id, bp) in blueprints {
        let spec = engine.register_blueprint(market_id.clone(), bp, registered_at);
        println!(
            "  [Live Fetch] {} -> {}",
            market_id, spec.primary_source_url
        );

        let mut urls = vec![spec.primary_source_url.clone()];
        urls.extend(spec.secondary_source_urls.clone());

        let mut fetched: Option<AttestedPayload> = None;
        let mut last_err = String::new();
        for u in &urls {
            match provider.fetch_attested(u).await {
                Ok(att) => {
                    fetched = Some(att);
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                }
            }
        }

        let attestation = fetched.ok_or_else(|| {
            format!(
                "Failed to fetch live oracle URL for '{}': {}",
                market_id, last_err
            )
        })?;

        let (report, fsm_state) = engine.ingest_general_attestation(&market_id, &attestation)?;
        println!(
            "    -> Attested {} bytes (SHA-256: {}) | Evaluated: {:?} -> Outcome={} | State={:?}",
            attestation.raw_bytes.len(),
            hex::encode(attestation.payload_sha256),
            report.evaluated_value,
            report.outcome,
            fsm_state
        );

        if let MarketLifecycleState::Proposed { expires_at, .. } = fsm_state {
            let receipt = engine.finalize_market(&market_id, expires_at + 1)?;
            println!(
                "    -> Finalized 153-byte Receipt: {} (Payout: {:?}/{})",
                hex::encode(receipt.receipt_hash()),
                receipt.payout_numerators,
                receipt.payout_denominator
            );
        }

        general_entries.push(GeneralCanaryEntry {
            market_id,
            spec,
            attestation,
            report,
        });
    }

    // Discover a live ESPN fixture across major leagues
    let candidate_leagues = [
        "soccer/eng.1",
        "basketball/nba",
        "soccer/esp.1",
        "soccer/uefa.champions",
        "baseball/mlb",
        "football/nfl",
    ];

    let mut sports_entry: Option<SportsCanaryEntry> = None;

    for sport_path in &candidate_leagues {
        println!("  [Live ESPN Scoreboard] Probing {} ...", sport_path);
        match provider.discover_live_espn_events(sport_path).await {
            Ok(events) if !events.is_empty() => {
                // Prefer a completed fixture so full-time settlement can be demonstrated immediately,
                // otherwise pick an in-play or scheduled fixture.
                let chosen = events
                    .iter()
                    .find(|e| e.completed)
                    .or_else(|| events.iter().find(|e| e.state == "in"))
                    .unwrap_or(&events[0]);

                println!(
                    "    -> Selected Live Event: {} ({}) [state={}, completed={}]",
                    chosen.name, chosen.event_id, chosen.state, chosen.completed
                );

                match provider
                    .fetch_espn_summary_attested(sport_path, &chosen.event_id)
                    .await
                {
                    Ok(attestation) => {
                        let raw_json = String::from_utf8_lossy(&attestation.raw_bytes).to_string();
                        match engine.register_sports_fixture_matrix(
                            sport_path,
                            &chosen.event_id,
                            &raw_json,
                            registered_at,
                        ) {
                            Ok(matrix) => {
                                let step_results = engine.ingest_sports_snapshot(
                                    sport_path,
                                    &chosen.event_id,
                                    &attestation,
                                )?;

                                let resolver = MultiplexedSportsResolver::default();
                                let report = resolver
                                    .resolve_matrix(
                                        sport_path,
                                        &chosen.event_id,
                                        &attestation,
                                        &matrix.templates,
                                    )
                                    .map_err(|e| e.to_string())?;

                                println!(
                                    "    -> Generated & Evaluated {} Derivative Markets from 1 Attested Summary ({} bytes):",
                                    matrix.templates.len(),
                                    attestation.raw_bytes.len()
                                );

                                for (mid, step) in &step_results {
                                    println!("       * {:<36} => {:?}", mid, step);
                                    if let Some(fsm) = engine.fsms.get(mid) {
                                        if let MarketLifecycleState::Proposed {
                                            expires_at, ..
                                        } = fsm.state
                                        {
                                            let _ = engine.finalize_market(mid, expires_at + 1);
                                        }
                                    }
                                }

                                sports_entry = Some(SportsCanaryEntry {
                                    sport_path: sport_path.to_string(),
                                    event_id: chosen.event_id.clone(),
                                    fixture_title: matrix.fixture_title,
                                    templates: matrix.templates,
                                    attestation,
                                    report,
                                });
                                break;
                            }
                            Err(e) => {
                                println!(
                                    "    -> Skipping event {} (summary normalization: {})",
                                    chosen.event_id, e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        println!("    -> Failed to fetch ESPN summary: {}", e);
                    }
                }
            }
            Ok(_) => {
                println!("    -> Scoreboard returned 0 events for {}", sport_path);
            }
            Err(e) => {
                println!("    -> Scoreboard probe error for {}: {}", sport_path, e);
            }
        }
    }

    Ok((general_entries, sports_entry))
}

async fn run_live_canary(out_path: &str) -> Result<(), String> {
    println!("=== RaaS Live Production Canary Runner ===");
    let mut engine = RaasEngine::new(180);

    let (general_markets, sports_fixture) = seed_engine_with_live_events(&mut engine).await?;

    engine
        .ledger
        .verify_chain_integrity()
        .map_err(|e| e.to_string())?;

    let mut receipts: Vec<SettlementReceipt> = engine.receipts.values().cloned().collect();
    receipts.sort_by(|a, b| a.market_id.cmp(&b.market_id));

    let bundle = LiveCanaryBundle {
        generated_at: Utc::now().timestamp(),
        oracle_pubkey_hex: hex::encode(engine.http_provider.public_key_bytes()),
        general_markets,
        sports_fixture,
        receipts,
        ledger: engine.ledger.clone(),
    };

    let json_bytes = serde_json::to_vec_pretty(&bundle).map_err(|e| e.to_string())?;
    fs::write(out_path, &json_bytes).map_err(|e| e.to_string())?;

    println!("\n=== Live Canary Complete ===");
    println!("  Oracle Authority PubKey : {}", bundle.oracle_pubkey_hex);
    println!(
        "  General Markets Settled : {}",
        bundle.general_markets.len()
    );
    println!(
        "  Sports Matrix Markets   : {}",
        bundle
            .sports_fixture
            .as_ref()
            .map(|s| s.templates.len())
            .unwrap_or(0)
    );
    println!("  On-Chain Receipts Issued: {}", bundle.receipts.len());
    println!(
        "  Audit Ledger Entries    : {} (Head: {})",
        bundle.ledger.entries.len(),
        hex::encode(bundle.ledger.head_hash())
    );
    println!(
        "  Saved Portable Bundle   : {} ({} bytes)",
        out_path,
        json_bytes.len()
    );

    Ok(())
}

fn run_generate_vectors(dir: &str) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let golden = build_golden_vectors_v1();
    let golden_path = format!("{}/golden_v1.json", dir);
    let golden_json = serde_json::to_vec_pretty(&golden).map_err(|e| e.to_string())?;
    fs::write(&golden_path, &golden_json).map_err(|e| e.to_string())?;

    let sample_bundle = build_deterministic_sample_canary_bundle();
    verify_canary_bundle(&sample_bundle)?;
    let sample_path = format!("{}/sample_canary_bundle.json", dir);
    let sample_json = serde_json::to_vec_pretty(&sample_bundle).map_err(|e| e.to_string())?;
    fs::write(&sample_path, &sample_json).map_err(|e| e.to_string())?;

    println!(
        "Generated deterministic golden vectors:\n  -> {} ({} bytes)\n  -> {} ({} bytes)",
        golden_path,
        golden_json.len(),
        sample_path,
        sample_json.len()
    );
    Ok(())
}

fn run_verify_bundle(path: &str) -> Result<(), String> {
    println!("=== Offline Zero-Trust Bundle Verifier ===");
    println!("  Reading bundle: {}", path);

    let raw = fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {}", path, e))?;
    let bundle: LiveCanaryBundle =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid bundle JSON: {}", e))?;

    println!("  Oracle PubKey : {}", bundle.oracle_pubkey_hex);

    verify_canary_bundle(&bundle)?;

    for entry in &bundle.general_markets {
        println!(
            "  [OK] General Market {:<26} | Outcome={} | Value={:?} | Commitment={}",
            entry.market_id,
            entry.report.outcome,
            entry.report.evaluated_value,
            hex::encode(entry.report.payload_commitment)
        );
    }

    if let Some(sports) = &bundle.sports_fixture {
        println!(
            "  [OK] Sports Matrix  {:<26} | {} Derivative Verdicts | Merkle={}",
            format!("{}:{}", sports.sport_path, sports.event_id),
            sports.report.verdicts.len(),
            hex::encode(sports.report.report_merkle_commitment)
        );
    }

    println!(
        "  [OK] Audit Ledger   {:<26} | {} Chained Entries | Head={}",
        "GENESIS -> HEAD",
        bundle.ledger.entries.len(),
        hex::encode(bundle.ledger.head_hash())
    );

    for receipt in &bundle.receipts {
        println!(
            "  [OK] 153B Receipt   {:<26} | Outcome={} | Payout={:?}/{} | Sig Verified",
            receipt.market_id,
            receipt.final_outcome,
            receipt.payout_numerators,
            receipt.payout_denominator
        );
    }

    println!("\nALL CRYPTOGRAPHIC PROOFS, MERKLE COMMITMENTS, LEDGER LINKS & 153-BYTE RECEIPTS VERIFIED OFFLINE.");
    Ok(())
}

async fn run_server(port: u16, seed_live: bool) -> Result<(), String> {
    let mut engine = RaasEngine::new(180);
    if seed_live {
        println!("Seeding RaaS engine with live network markets and ESPN fixtures...");
        match seed_engine_with_live_events(&mut engine).await {
            Ok((gen, sports)) => {
                println!(
                    "Seeded {} general markets and {} sports fixture matrix.",
                    gen.len(),
                    if sports.is_some() { 1 } else { 0 }
                );
            }
            Err(e) => {
                eprintln!("Warning: live seeding encountered an error: {}", e);
            }
        }
    }

    let state = Arc::new(RwLock::new(engine));
    let app = build_router(state);
    let addr = format!("0.0.0.0:{}", port);
    println!("=== RaaS HTTP API Listening on http://{} ===", addr);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| e.to_string())?;
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}

fn run_offline_demo() {
    println!("=== Verifiable Resolution-as-a-Service (RaaS) Engine ===\n");

    let authority = MockAttestationAuthority::new();
    let cutoff = Utc::now();

    println!("[1] General AST Blueprint Resolution:");
    let blueprint = MarketBlueprint::CryptoSpotCross {
        symbol: "BTCUSDT".to_string(),
        strike_price: Decimal::from(70000),
        operator: ComparisonOperator::GreaterThan,
        cutoff_time: cutoff,
    };

    let spec = blueprint.compile();
    println!("  Market Question: {}", spec.question);
    println!("  Primary Oracle URL: {}", spec.primary_source_url);

    let raw_payload = br#"{"symbol":"BTCUSDT","price":"71250.50"}"#;
    let attested = authority.create_attested_payload(
        &spec.primary_source_url,
        raw_payload,
        cutoff.timestamp(),
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("date".to_string(), cutoff.to_rfc2822()),
            (
                "etag".to_string(),
                "\"d41d8cd98f00b204e9800998ecf8427e\"".to_string(),
            ),
        ],
    );

    match verify_resolution_offline("btc-70k-cutoff", &spec, &attested, cutoff.timestamp()) {
        Ok(report) => {
            println!("  -> Verified Outcome: {} (1=YES, 2=NO)", report.outcome);
            println!(
                "  -> Commitment Hash: {}\n",
                hex::encode(report.payload_commitment)
            );
        }
        Err(e) => {
            eprintln!("  -> Verification failed: {}\n", e);
        }
    }

    println!(
        "[2] Multiplexed Sports Fixture Matrix Resolution (1 Attestation -> 8 Derivative Markets):"
    );
    let espn_summary_bytes = br#"{
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

    let fixture = NormalizedFixtureState::from_espn_summary_json(
        "soccer/eng.1",
        "401696",
        std::str::from_utf8(espn_summary_bytes).unwrap(),
    )
    .expect("valid ESPN fixture JSON");

    let matrix = FixtureMarketMatrix::generate_from_fixture(&fixture);
    println!(
        "  Fixture: {} (Event ID: {}, Generated {} Derivative Markets)",
        matrix.fixture_title,
        matrix.event_id,
        matrix.templates.len()
    );

    let espn_attestation = authority.create_attested_payload(
        "https://site.api.espn.com/apis/site/v2/sports/soccer/eng.1/summary?event=401696",
        espn_summary_bytes,
        cutoff.timestamp(),
        vec![("content-type".into(), "application/json".into())],
    );

    let resolver = MultiplexedSportsResolver::default();
    let sports_report = resolver
        .resolve_matrix(
            "soccer/eng.1",
            "401696",
            &espn_attestation,
            &matrix.templates,
        )
        .expect("multiplexed sports resolution");

    for v in &sports_report.verdicts {
        let label = match v.outcome_code {
            1 => "YES (1)",
            2 => "NO  (2)",
            3 => "VOID(3)",
            _ => "PEND(0)",
        };
        println!("  -> [{}] {:<38} | {}", label, v.template_key, v.question);
    }

    resolver
        .verify_report_offline(&espn_attestation, &matrix.templates, &sports_report)
        .expect("offline multiplexed report verification");
    println!(
        "  -> Offline Merkle Report Commitment Verified: {}",
        hex::encode(sports_report.report_merkle_commitment)
    );
}

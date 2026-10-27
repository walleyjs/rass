use chrono::Utc;
use rust_decimal::Decimal;

use resolution_service::ast::ComparisonOperator;
use resolution_service::blueprints::MarketBlueprint;
use resolution_service::provenance::mock::MockAttestationAuthority;
use resolution_service::sports::{
    FixtureMarketMatrix, MultiplexedSportsResolver, NormalizedFixtureState,
};
use resolution_service::verifier::verify_resolution_offline;

fn main() {
    println!("=== Verifiable Resolution-as-a-Service (RaaS) Engine ===\n");

    let authority = MockAttestationAuthority::new();
    let cutoff = Utc::now();

    // -------------------------------------------------------------------------
    // PART 1: General AST Blueprint Resolution (CryptoSpotCross)
    // -------------------------------------------------------------------------
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
            ("etag".to_string(), "\"d41d8cd98f00b204e9800998ecf8427e\"".to_string()),
        ],
    );

    match verify_resolution_offline("btc-70k-cutoff", &spec, &attested, cutoff.timestamp()) {
        Ok(report) => {
            println!("  -> Verified Outcome: {} (1=YES, 2=NO)", report.outcome);
            println!("  -> Commitment Hash: {}\n", hex::encode(report.payload_commitment));
        }
        Err(e) => {
            eprintln!("  -> Verification failed: {}\n", e);
        }
    }

    // -------------------------------------------------------------------------
    // PART 2: Multiplexed Sports Fixture Matrix Resolution (1 Fetch -> 8 Markets)
    // -------------------------------------------------------------------------
    println!("[2] Multiplexed Sports Fixture Matrix Resolution (1 Attestation -> 8 Derivative Markets):");
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
        .resolve_matrix("soccer/eng.1", "401696", &espn_attestation, &matrix.templates)
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

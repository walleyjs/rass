use chrono::Utc;
use rust_decimal::Decimal;

use resolution_service::ast::ComparisonOperator;
use resolution_service::blueprints::MarketBlueprint;
use resolution_service::provenance::mock::MockAttestationAuthority;
use resolution_service::verifier::verify_resolution_offline;

fn main() {
    println!("=== Verifiable Resolution-as-a-Service (RaaS) Engine ===");

    let authority = MockAttestationAuthority::new();
    let cutoff = Utc::now();

    // 1. Define institutional blueprint
    let blueprint = MarketBlueprint::CryptoSpotCross {
        symbol: "BTCUSDT".to_string(),
        strike_price: Decimal::from(70000),
        operator: ComparisonOperator::GreaterThan,
        cutoff_time: cutoff,
    };

    let spec = blueprint.compile();
    println!("Market Question: {}", spec.question);
    println!("Primary Oracle URL: {}", spec.primary_source_url);

    // 2. Simulate attested response payload from primary oracle
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

    // 3. Perform 100% offline cryptographic & AST verification
    match verify_resolution_offline("btc-70k-cutoff", &spec, &attested, cutoff.timestamp()) {
        Ok(report) => {
            println!("Resolution Succeeded!");
            println!("  Market ID: {}", report.market_id);
            println!("  Outcome: {} (1=YES, 2=NO)", report.outcome);
            println!("  Evaluated Value: {:?}", report.evaluated_value);
            println!("  Commitment Hash: {}", hex::encode(report.payload_commitment));
        }
        Err(e) => {
            eprintln!("Resolution verification failed: {}", e);
        }
    }
}

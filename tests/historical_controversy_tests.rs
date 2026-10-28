use chrono::Utc;
use rust_decimal::Decimal;

use resolution_service::ast::{ComparisonOperator, EvaluationContext, Expression};
use resolution_service::blueprints::MarketBlueprint;
use resolution_service::fsm::{
    Bond, EscalationLadder, FsmError, MarketLifecycleState, ResolutionFsm, TemporalRules,
};
use resolution_service::provenance::mock::MockAttestationAuthority;
use resolution_service::verifier::{verify_resolution_offline, VerifierError};

/// ARCHETYPE 1: Upstream Bureau / Agency Retroactive Data Revision
/// A government statistical agency (like BLS or Eurostat) or sports league revises official numbers
/// hours or days after the initial release.
/// Expected institutional behavior: The engine enforces a strict temporal freeze window. Revisions
/// past the freeze deadline are hard-rejected, protecting finalized settlements from retrospective exploitation.
#[test]
fn test_controversy_archetype_retroactive_data_revision() {
    let cutoff = 1_700_000_000;
    let freeze_seconds = 3600; // 1-hour freeze window
    let temporal = TemporalRules {
        cutoff_time: cutoff,
        challenge_window_seconds: 7200,
        retroactive_freeze_seconds: freeze_seconds,
    };

    let ladder = EscalationLadder::default();
    let mut fsm = ResolutionFsm::new("cpi-nov-indicator", temporal, ladder);

    // Initial proposal at cutoff + 10s based on first release (outcome 1 = YES)
    let bond = Bond {
        currency: "USDC".into(),
        amount: Decimal::from(100),
        bonded_party: "oracle_node_1".into(),
    };

    fsm.propose(1, bond, [1u8; 32], cutoff + 10).unwrap();

    // 1. Revision occurs at cutoff + 1800s (30 mins in, before freeze window expires)
    let pre_freeze_revision = fsm.evaluate_source_revision(cutoff + 1800, 2);
    assert_eq!(
        pre_freeze_revision,
        Ok(true),
        "Revision within freeze window must be flagged for re-evaluation"
    );

    // 2. Revision occurs at cutoff + 4000s (past the 3600s freeze window)
    let post_freeze_revision = fsm.evaluate_source_revision(cutoff + 4000, 2);
    assert!(
        matches!(
            post_freeze_revision,
            Err(FsmError::RetroactiveChangeFrozen { .. })
        ),
        "Revision after freeze deadline MUST be rejected by FSM"
    );
}

/// ARCHETYPE 2: Mid-Flight API Schema Drift & Incomplete Payloads
/// A Web2 data provider updates its response JSON structure without notice (e.g., wrapping "price"
/// inside a nested "data.ticker" object) or returns an HTTP 200 containing an error envelope.
/// Expected institutional behavior: Engine fails-closed. It never guesses or coerces missing fields
/// to default 0/null values, and offline verification returns an explicit typed error.
#[test]
fn test_controversy_archetype_midflight_schema_drift() {
    let authority = MockAttestationAuthority::new();
    let cutoff = Utc::now();

    let blueprint = MarketBlueprint::CryptoSpotCross {
        symbol: "BTCUSDT".to_string(),
        strike_price: Decimal::from(70000),
        operator: ComparisonOperator::GreaterThan,
        cutoff_time: cutoff,
    };

    let spec = blueprint.compile();

    // Drifting schema: Provider renamed "price" to "current_rate"
    let drifted_json = br#"{"symbol":"BTCUSDT","current_rate":"72500.00"}"#;
    let attested = authority.create_attested_payload(
        &spec.primary_source_url,
        drifted_json,
        cutoff.timestamp(),
        vec![("content-type".to_string(), "application/json".to_string())],
    );

    // Offline verifier must fail-closed with JsonPathError
    let verification_result =
        verify_resolution_offline("btc-drift-market", &spec, &attested, cutoff.timestamp());
    assert!(
        matches!(verification_result, Err(VerifierError::AstError(_))),
        "Schema drift must fail-closed with AST error"
    );
}

/// ARCHETYPE 3: Multi-Witness Oracle Divergence / Network Partition
/// Two trusted multi-witness sources (e.g., Primary Binance vs Secondary Mirror) disagree on price
/// or event status at cutoff.
/// Expected institutional behavior: Consensus evaluation detects agreement failure (< threshold),
/// refuses to settle unilaterally, and halts the automated proposal.
#[test]
fn test_controversy_archetype_multi_witness_divergence() {
    // Strategy 1 extracts from Primary ("71000")
    let strat1 = Expression::ParseDecimal {
        target: Box::new(Expression::JsonPath {
            path: "primary_price".to_string(),
            target: Box::new(Expression::PayloadRoot),
        }),
    };

    // Strategy 2 extracts from Secondary ("68500")
    let strat2 = Expression::ParseDecimal {
        target: Box::new(Expression::JsonPath {
            path: "secondary_price".to_string(),
            target: Box::new(Expression::PayloadRoot),
        }),
    };

    // Require 100% agreement between witnesses
    let consensus_ast = Expression::MultiWitnessConsensus {
        strategies: vec![strat1, strat2],
        min_agreement_ratio: Decimal::from(1),
    };

    let divergent_payload = r#"{"primary_price":"71000.00","secondary_price":"68500.00"}"#;
    let ctx = EvaluationContext::new(divergent_payload);

    let result = consensus_ast.evaluate(&ctx);
    assert!(
        result.is_err(),
        "Divergent witnesses must fail consensus when agreement < 1.0"
    );
}

/// DISPUTE ESCALATION & BOND SLOGGING TEST
/// Verifies the economic bond escalation ladder prevents griefing and terminates into finalized resolution.
#[test]
fn test_dispute_escalation_ladder_and_settlement() {
    let cutoff = 1_700_000_000;
    let temporal = TemporalRules {
        cutoff_time: cutoff,
        challenge_window_seconds: 3600,
        retroactive_freeze_seconds: 600,
    };
    let ladder = EscalationLadder {
        initial_bond: Decimal::from(100),
        multiplier: Decimal::from(2),
        max_rounds: 3,
    };

    let mut fsm = ResolutionFsm::new("escalation-test", temporal, ladder);

    // 1. Propose YES (outcome 1) with 100 USDC bond
    let bond_proposer = Bond {
        currency: "USDC".into(),
        amount: Decimal::from(100),
        bonded_party: "alice".into(),
    };
    fsm.propose(1, bond_proposer, [0u8; 32], cutoff).unwrap();

    // 2. Bob challenges with counter-outcome NO (outcome 2). Required bond = 100 * 2^1 = 200 USDC
    let underfunded_bond = Bond {
        currency: "USDC".into(),
        amount: Decimal::from(150),
        bonded_party: "bob".into(),
    };
    assert!(fsm.challenge(2, underfunded_bond, cutoff + 100).is_err());

    let bob_bond = Bond {
        currency: "USDC".into(),
        amount: Decimal::from(200),
        bonded_party: "bob".into(),
    };
    fsm.challenge(2, bob_bond, cutoff + 100).unwrap();

    // 3. Challenge window passes without further challenges. Finalize settled outcome.
    let finalized_outcome = fsm.finalize(cutoff + 100 + 3601).unwrap();
    assert_eq!(
        finalized_outcome, 2,
        "Bob's counter-outcome must win upon challenge window expiration"
    );

    match fsm.state {
        MarketLifecycleState::Finalized { winning_party, .. } => {
            assert_eq!(winning_party, "bob");
        }
        _ => panic!("State must be Finalized"),
    }
}

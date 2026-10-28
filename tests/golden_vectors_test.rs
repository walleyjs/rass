use std::fs;

use resolution_service::bundle::{
    build_deterministic_sample_canary_bundle, build_golden_vectors_v1, verify_canary_bundle,
    GoldenTestVectorsV1, LiveCanaryBundle,
};
use resolution_service::receipt::{verify_onchain_receipt, RECEIPT_WIRE_LEN};

#[test]
fn test_golden_vectors_v1_bit_for_bit_reproducibility() {
    let raw = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/vectors/golden_v1.json"
    ))
    .expect("vectors/golden_v1.json must exist in repository");
    let loaded: GoldenTestVectorsV1 =
        serde_json::from_str(&raw).expect("valid GoldenTestVectorsV1 JSON");

    let recomputed = build_golden_vectors_v1();
    assert_eq!(
        loaded, recomputed,
        "Committed vectors/golden_v1.json drifted from deterministic seed [0x42; 32]!"
    );

    // Explicitly verify cryptographic invariants on loaded vectors
    assert!(loaded
        .attestation_vector
        .attested_payload
        .verify_integrity()
        .is_ok());

    assert_eq!(loaded.receipt_vectors.len(), 3);
    for rv in &loaded.receipt_vectors {
        let wire_bytes = hex::decode(&rv.wire_bytes_hex).expect("valid wire hex");
        assert_eq!(wire_bytes.len(), RECEIPT_WIRE_LEN);
        assert_eq!(&wire_bytes[0..24], b"RAAS_ONCHAIN_RECEIPT_V1:");
        assert!(verify_onchain_receipt(&rv.receipt).is_ok());
    }

    assert!(loaded.ledger_vector.ledger.verify_chain_integrity().is_ok());
}

#[test]
fn test_sample_canary_bundle_offline_verification() {
    let raw = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/vectors/sample_canary_bundle.json"
    ))
    .expect("vectors/sample_canary_bundle.json must exist in repository");
    let loaded: LiveCanaryBundle = serde_json::from_str(&raw).expect("valid LiveCanaryBundle JSON");

    let recomputed = build_deterministic_sample_canary_bundle();
    assert_eq!(
        loaded, recomputed,
        "Committed vectors/sample_canary_bundle.json drifted from deterministic seed [0x42; 32]!"
    );

    verify_canary_bundle(&loaded).expect("offline verification of sample_canary_bundle must pass");
    assert_eq!(loaded.general_markets.len(), 3);
    assert_eq!(
        loaded
            .sports_fixture
            .as_ref()
            .expect("sports fixture present")
            .templates
            .len(),
        10
    );
    assert_eq!(loaded.receipts.len(), 13);
}

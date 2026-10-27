use proptest::prelude::*;
use rust_decimal::Decimal;

use resolution_service::ast::{
    semver::ParsedSemVer, ComparisonOperator, EvaluationContext, Expression, Value,
};
use resolution_service::provenance::mock::MockAttestationAuthority;

proptest! {
    /// Fuzz test: Arbitrary payload input into JsonPath extractor must never panic
    #[test]
    fn fuzz_json_path_never_panics(payload in ".*", path in "[a-zA-Z0-9_.\\[\\]]{1,30}") {
        let ast = Expression::JsonPath {
            path,
            target: Box::new(Expression::PayloadRoot),
        };
        let ctx = EvaluationContext::new(payload);
        let _ = ast.evaluate(&ctx);
    }

    /// Fuzz test: Arbitrary regex patterns against arbitrary text must never panic
    #[test]
    fn fuzz_regex_eval_never_panics(pattern in "[a-zA-Z0-9.+*?()|\\[\\]]{1,20}", text in ".*") {
        let ast = Expression::RegexMatch {
            pattern,
            capture_group: 0,
            target: Box::new(Expression::PayloadRoot),
        };
        let ctx = EvaluationContext::new(text);
        let _ = ast.evaluate(&ctx);
    }

    /// Fuzz test: Fixed-point decimal arithmetic consistency & monotonicity
    #[test]
    fn fuzz_decimal_monotonicity(a in -1_000_000i64..1_000_000i64, b in -1_000_000i64..1_000_000i64) {
        let dec_a = Decimal::from(a);
        let dec_b = Decimal::from(b);

        let ast = Expression::Compare {
            op: ComparisonOperator::GreaterThan,
            left: Box::new(Expression::Literal { value: Value::Decimal(dec_a) }),
            right: Box::new(Expression::Literal { value: Value::Decimal(dec_b) }),
        };

        let ctx = EvaluationContext::new("");
        let res = ast.evaluate(&ctx).unwrap();
        prop_assert_eq!(res, Value::Boolean(a > b));
    }

    /// Fuzz test: SemVer ordering is strictly deterministic across arbitrary version triplets
    #[test]
    fn fuzz_semver_ordering(
        m1 in 0u64..100, n1 in 0u64..100, p1 in 0u64..100,
        m2 in 0u64..100, n2 in 0u64..100, p2 in 0u64..100,
    ) {
        let sv1_str = format!("{}.{}.{}", m1, n1, p1);
        let sv2_str = format!("{}.{}.{}", m2, n2, p2);

        let sv1 = ParsedSemVer::parse(&sv1_str).unwrap();
        let sv2 = ParsedSemVer::parse(&sv2_str).unwrap();

        let ast = Expression::Compare {
            op: ComparisonOperator::GreaterThanOrEqual,
            left: Box::new(Expression::Literal { value: Value::SemVer(sv1.clone()) }),
            right: Box::new(Expression::Literal { value: Value::SemVer(sv2.clone()) }),
        };

        let ctx = EvaluationContext::new("");
        let res = ast.evaluate(&ctx).unwrap();
        let expected = (m1, n1, p1) >= (m2, n2, p2);
        prop_assert_eq!(res, Value::Boolean(expected));
    }

    /// Fuzz test: Attested payload tamper-evidence. Mutating any byte breaks cryptographic integrity.
    #[test]
    fn fuzz_attestation_tamper_evidence(
        body in proptest::collection::vec(any::<u8>(), 1..100),
        mutate_idx in 0usize..99,
        mutate_byte in 1u8..255
    ) {
        let authority = MockAttestationAuthority::new();
        let mut attested = authority.create_attested_payload(
            "https://api.source.org/data",
            &body,
            1700000000,
            vec![("etag".to_string(), "123".to_string())],
        );

        // Untampered payload must verify
        prop_assert!(attested.verify_integrity().is_ok());

        // Mutate body byte
        let idx = mutate_idx % attested.raw_bytes.len();
        attested.raw_bytes[idx] ^= mutate_byte;

        // Tampered payload MUST fail verification
        prop_assert!(attested.verify_integrity().is_err());
    }
}

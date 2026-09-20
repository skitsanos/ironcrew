use super::*;
use ironcrew::usage::{UsageSnapshot, UsageTracker};

fn snapshot() -> UsageSnapshot {
    let scope = UsageTracker::default();
    scope.start().unwrap().finish(receipt(u64::MAX, 0)).unwrap();
    scope.snapshot().unwrap()
}

#[test]
fn full_unsigned_range_roundtrips_as_strings_with_explicit_nulls() {
    let snapshot = snapshot();
    let wire = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(wire["settled"]["requests"], "1");
    assert_eq!(wire["in_flight"], "0");
    assert_eq!(
        wire["settled"]["total_tokens"]["known"],
        u64::MAX.to_string()
    );
    assert_eq!(wire["settled"]["cached_tokens"]["known"], json!(null));
    assert_eq!(wire["settled"]["cached_tokens"]["complete"], false);
    assert_eq!(
        serde_json::from_value::<UsageSnapshot>(wire).unwrap(),
        snapshot
    );
    let receipt = receipt(u64::MAX, 0);
    assert_eq!(
        serde_json::from_value::<UsageReceipt>(serde_json::to_value(&receipt).unwrap()).unwrap(),
        receipt
    );
}

#[test]
fn numeric_noncanonical_and_overflowing_wire_counts_are_rejected() {
    for invalid in [
        json!(1),
        json!(1.0),
        json!(true),
        json!(""),
        json!("01"),
        json!("+1"),
        json!("-1"),
        json!(" 1"),
        json!("1.0"),
        json!("1e1"),
        json!("18446744073709551616"),
        json!("１２"),
    ] {
        for path in [
            "/in_flight",
            "/settled/requests",
            "/settled/total_tokens/known",
        ] {
            let mut wire = serde_json::to_value(snapshot()).unwrap();
            *wire.pointer_mut(path).unwrap() = invalid.clone();
            assert!(
                serde_json::from_value::<UsageSnapshot>(wire).is_err(),
                "{path}: {invalid}"
            );
        }
    }
}

#[test]
fn forged_coverage_counts_subsets_and_unknown_fields_are_rejected() {
    let base = serde_json::to_value(snapshot()).unwrap();
    for (path, value) in [
        ("/coverage", json!("partial")),
        ("/in_flight", json!("1")),
        ("/settled/requests", json!("0")),
        ("/settled/coverage", json!("partial")),
        ("/settled/cached_tokens/complete", json!(true)),
        ("/settled/total_tokens/known", json!("1")),
        ("/settled/reasoning_tokens/known", json!("1")),
        ("/settled/completion_tokens/known", json!("1")),
    ] {
        let mut wire = base.clone();
        *wire.pointer_mut(path).unwrap() = value;
        assert!(
            serde_json::from_value::<UsageSnapshot>(wire).is_err(),
            "{path}"
        );
    }
    for path in ["", "/settled", "/settled/total_tokens"] {
        let mut wire = base.clone();
        wire.pointer_mut(path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(0));
        assert!(serde_json::from_value::<UsageSnapshot>(wire).is_err());
    }
    let mut wire = base;
    wire["settled"]["requests"] = json!(u64::MAX.to_string());
    wire["in_flight"] = json!("1");
    wire["coverage"] = json!("partial");
    assert!(serde_json::from_value::<UsageSnapshot>(wire).is_err());
}

#[test]
fn unknown_checkpoint_is_not_an_empty_complete_scope() {
    let missing = UsageSnapshot::unavailable();
    assert_eq!(missing.coverage, UsageCoverage::Unavailable);
    assert_eq!(missing.settled.total_tokens().known(), None);
    assert_eq!(
        serde_json::from_value::<UsageSnapshot>(serde_json::to_value(&missing).unwrap()).unwrap(),
        missing
    );
    let mut aggregate = missing.settled;
    aggregate.merge(&UsageAggregate::default()).unwrap();
    aggregate.add(&receipt(2, 1)).unwrap();
    assert_eq!(aggregate.coverage(), UsageCoverage::Partial);
    assert_eq!(aggregate.total_tokens().known(), Some(3));
    assert!(!aggregate.total_tokens().complete());
}

#[test]
fn absent_fields_and_lower_bounds_above_complete_total_are_rejected() {
    let base = serde_json::to_value(snapshot()).unwrap();
    for (path, key) in [
        ("", "in_flight"),
        ("/settled", "requests"),
        ("/settled/total_tokens", "known"),
        ("/settled/total_tokens", "complete"),
    ] {
        let mut wire = base.clone();
        wire.pointer_mut(path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(
            serde_json::from_value::<UsageSnapshot>(wire).is_err(),
            "{path}/{key}"
        );
    }
    let mut wire = base;
    wire["coverage"] = json!("partial");
    wire["settled"]["coverage"] = json!("partial");
    wire["settled"]["prompt_tokens"] = json!({"known":"10","complete":false});
    wire["settled"]["completion_tokens"] = json!({"known":"10","complete":false});
    wire["settled"]["total_tokens"]["known"] = json!("15");
    assert!(serde_json::from_value::<UsageSnapshot>(wire).is_err());
}

#[test]
fn differently_covered_receipts_roundtrip_without_comparing_unrelated_subtotals() {
    let variants = [
        json!({"input_tokens":10}),
        json!({"output_tokens":7}),
        json!({"input_tokens":20,"output_tokens":3,"total_tokens":23}),
        json!({"total_tokens":1}),
        json!(null),
        json!({"output_tokens_details":{"reasoning_tokens":5}}),
    ];
    for first in &variants {
        for second in &variants {
            let mut aggregate = UsageAggregate::default();
            for input in [first, second] {
                aggregate
                    .add(&ProviderUsage::OpenAiResponses.parse(Some(input), true))
                    .unwrap();
            }
            let wire = serde_json::to_value(&aggregate).unwrap();
            assert_eq!(
                serde_json::from_value::<UsageAggregate>(wire).unwrap(),
                aggregate
            );
        }
    }
}

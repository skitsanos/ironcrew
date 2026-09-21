//! IC-046 receipt foundation, not evidence of runtime/store integration.
use ironcrew::usage::{ProviderUsage, StreamUsage, UsageAggregate, UsageCoverage, UsageReceipt};
use serde_json::json;

#[path = "usage_accounting/hierarchy.rs"]
mod hierarchy;
#[path = "usage_accounting/tracker.rs"]
mod tracker;
#[path = "usage_accounting/wire.rs"]
mod wire;

fn receipt(prompt: u64, output: u64) -> UsageReceipt {
    ProviderUsage::OpenAiResponses.parse(
        Some(&json!({"input_tokens":prompt,"output_tokens":output,"total_tokens":prompt+output})),
        true,
    )
}

#[test]
fn missing_usage_is_not_a_zero_receipt() {
    let unknown = ProviderUsage::OpenAiResponses.parse(None, true);
    assert_eq!(unknown.coverage(), UsageCoverage::Unavailable);
    assert_eq!(unknown.counts().total_tokens, None);
    assert_eq!(
        serde_json::to_value(unknown).unwrap()["counts"]["total_tokens"],
        json!(null)
    );
    let zero = receipt(0, 0);
    assert_eq!(zero.coverage(), UsageCoverage::Complete);
    assert_eq!(zero.counts().total_tokens, Some(0));
}

#[test]
fn large_receipt_keeps_reasoning_and_cache_subsets_without_double_counting() {
    let usage = ProviderUsage::OpenAiResponses.parse(
        Some(&json!({
            "input_tokens": 5_000_000_000_u64, "output_tokens": 20,
            "total_tokens": 5_000_000_020_u64,
            "input_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 50},
            "output_tokens_details": {"reasoning_tokens": 12}
        })),
        true,
    );
    assert_eq!(usage.coverage(), UsageCoverage::Complete);
    assert_eq!(usage.counts().total_tokens, Some(5_000_000_020));
    assert_eq!(usage.counts().reasoning_tokens, Some(12));
    assert_eq!(usage.counts().cached_tokens, Some(40));
    assert_eq!(usage.counts().cache_write_tokens, Some(50));
}

#[test]
fn failed_attempt_and_successful_retry_keep_known_subtotals() {
    let failed = ProviderUsage::OpenAiResponses.parse(Some(&json!({"input_tokens":10})), false);
    let mut aggregate = UsageAggregate::default();
    aggregate.add(&failed).unwrap();
    aggregate.add(&receipt(20, 5)).unwrap();
    assert_eq!(aggregate.requests(), 2);
    assert_eq!(aggregate.coverage(), UsageCoverage::Partial);
    assert_eq!(aggregate.prompt_tokens().known(), Some(30));
    assert!(!aggregate.prompt_tokens().complete());
    assert_eq!(aggregate.total_tokens().known(), Some(25));
    assert!(!aggregate.total_tokens().complete());
}

#[test]
fn anthropic_stream_replaces_cumulative_output_instead_of_adding_deltas() {
    let mut stream = StreamUsage::new(ProviderUsage::Anthropic);
    stream.update(&json!({"input_tokens":10,"cache_creation_input_tokens":4,
        "cache_read_input_tokens":6,"output_tokens":1}));
    stream.update(&json!({"output_tokens":3}));
    stream.update(&json!({"output_tokens":5,"output_tokens_details":{"thinking_tokens":2}}));
    let usage = stream.finish(true);
    assert_eq!(usage.coverage(), UsageCoverage::Complete);
    assert_eq!(usage.counts().prompt_tokens, Some(20));
    assert_eq!(usage.counts().completion_tokens, Some(5));
    assert_eq!(usage.counts().total_tokens, Some(25));
    assert_eq!(usage.counts().reasoning_tokens, Some(2));
}

#[test]
fn chat_completion_detail_mapping_matches_responses_mapping() {
    let usage = ProviderUsage::OpenAiChat.parse(
        Some(&json!({
            "prompt_tokens": 20, "completion_tokens": 7, "total_tokens":27,
            "prompt_tokens_details":{"cached_tokens":12,"cache_write_tokens":3},
            "completion_tokens_details":{"reasoning_tokens":4}
        })),
        true,
    );
    assert_eq!(usage.coverage(), UsageCoverage::Complete);
    assert_eq!(usage.counts().reasoning_tokens, Some(4));
    assert_eq!(usage.counts().cached_tokens, Some(12));
    assert_eq!(usage.counts().cache_write_tokens, Some(3));
}

#[test]
fn malformed_counts_are_unknown_not_zero() {
    for value in [
        json!(-1),
        json!(1.5),
        json!("12"),
        json!(true),
        json!([]),
        json!({}),
        json!(null),
    ] {
        let usage = ProviderUsage::OpenAiResponses.parse(
            Some(&json!({
                "input_tokens":value,"output_tokens":5,"total_tokens":15
            })),
            true,
        );
        assert_eq!(usage.coverage(), UsageCoverage::Partial);
        assert_eq!(usage.counts().prompt_tokens, None);
        assert_eq!(usage.counts().completion_tokens, Some(5));
    }
    let too_large = serde_json::from_str("{\"input_tokens\":18446744073709551616}").unwrap();
    assert_eq!(
        ProviderUsage::OpenAiResponses
            .parse(Some(&too_large), true)
            .counts()
            .prompt_tokens,
        None
    );
}

#[test]
fn malformed_detail_containers_are_not_scalar_counts() {
    for container in [json!(4), json!("four"), json!(null), json!([])] {
        let usage = ProviderUsage::OpenAiResponses.parse(
            Some(&json!({
                "input_tokens":20,"output_tokens":5,"total_tokens":25,
                "output_tokens_details":container
            })),
            true,
        );
        assert_eq!(usage.counts().reasoning_tokens, None);
        assert_eq!(usage.coverage(), UsageCoverage::Partial);
    }
}

#[test]
fn inconsistent_totals_and_subsets_do_not_make_complete_receipts() {
    let usage = ProviderUsage::OpenAiResponses.parse(
        Some(&json!({
            "input_tokens":20,"output_tokens":5,"total_tokens":24,
            "input_tokens_details":{"cached_tokens":21,"cache_write_tokens":22},
            "output_tokens_details":{"reasoning_tokens":6}
        })),
        true,
    );
    assert_eq!(usage.coverage(), UsageCoverage::Partial);
    assert_eq!(usage.counts().prompt_tokens, Some(20));
    assert_eq!(usage.counts().completion_tokens, Some(5));
    assert_eq!(usage.counts().total_tokens, None);
    assert_eq!(usage.counts().cached_tokens, None);
    assert_eq!(usage.counts().cache_write_tokens, None);
    assert_eq!(usage.counts().reasoning_tokens, None);
}

#[test]
fn missing_optional_details_do_not_invent_zero_or_hide_primary_coverage() {
    let mut aggregate = UsageAggregate::default();
    aggregate.add(&receipt(10, 5)).unwrap();
    assert_eq!(aggregate.coverage(), UsageCoverage::Complete);
    assert_eq!(aggregate.reasoning_tokens().known(), None);
    assert!(!aggregate.reasoning_tokens().complete());
    assert_eq!(aggregate.cached_tokens().known(), None);
    assert!(!aggregate.cached_tokens().complete());
}

#[test]
fn unavailable_requests_survive_aggregation_and_serialization() {
    let mut aggregate = UsageAggregate::default();
    aggregate.add(&UsageReceipt::default()).unwrap();
    aggregate.add(&UsageReceipt::default()).unwrap();
    assert_eq!(aggregate.requests(), 2);
    assert_eq!(aggregate.coverage(), UsageCoverage::Unavailable);
    let value = serde_json::to_value(&aggregate).unwrap();
    assert_eq!(value["total_tokens"]["known"], json!(null));
    assert_eq!(value["total_tokens"]["complete"], json!(false));
    aggregate.add(&receipt(10, 5)).unwrap();
    assert_eq!(aggregate.total_tokens().known(), Some(15));
    assert_eq!(aggregate.coverage(), UsageCoverage::Partial);
    assert!(!aggregate.total_tokens().complete());
}

#[test]
fn empty_aggregate_is_an_identity_not_an_unavailable_attempt() {
    let empty = UsageAggregate::default();
    assert_eq!(empty.requests(), 0);
    assert_eq!(empty.total_tokens().known(), Some(0));
    let mut usage = empty.clone();
    usage.add(&UsageReceipt::default()).unwrap();
    let before = usage.clone();
    usage.merge(&empty).unwrap();
    assert_eq!(usage, before);
}

#[test]
fn arithmetic_overflow_never_wraps_saturates_or_partially_mutates() {
    let mut usage = UsageAggregate::default();
    usage.add(&receipt(u64::MAX, 0)).unwrap();
    let before = usage.clone();
    assert!(usage.add(&receipt(1, 1)).is_err());
    assert_eq!(usage, before);
    let mut attempts = UsageAggregate::default();
    attempts.add(&receipt(0, 0)).unwrap();
    for _ in 0..63 {
        attempts.merge(&attempts.clone()).unwrap();
    }
    let before = attempts.clone();
    assert!(attempts.merge(&before).is_err());
    assert_eq!(attempts, before);
}

#[test]
fn per_receipt_arithmetic_overflow_does_not_claim_a_total() {
    let usage = ProviderUsage::OpenAiResponses.parse(
        Some(&json!({
            "input_tokens":u64::MAX,"output_tokens":1,"total_tokens":0
        })),
        true,
    );
    assert_eq!(usage.counts().total_tokens, None);
    assert_eq!(usage.coverage(), UsageCoverage::Partial);
    let usage = ProviderUsage::Anthropic.parse(
        Some(&json!({
            "input_tokens":u64::MAX,"cache_creation_input_tokens":1,
            "cache_read_input_tokens":0,"output_tokens":2
        })),
        true,
    );
    assert_eq!(usage.counts().prompt_tokens, None);
    assert_ne!(usage.coverage(), UsageCoverage::Complete);
    assert_eq!(usage.counts().total_tokens, None);
}

#[test]
fn interrupted_stream_and_null_chunks_preserve_known_receipts() {
    for provider in [ProviderUsage::OpenAiChat, ProviderUsage::OpenAiResponses] {
        let mut stream = StreamUsage::new(provider);
        let usage = match provider {
            ProviderUsage::OpenAiChat => {
                json!({"prompt_tokens":10,"completion_tokens":5,"total_tokens":15})
            }
            _ => json!({"input_tokens":10,"output_tokens":5,"total_tokens":15}),
        };
        stream.update(&usage);
        stream.update(&json!(null));
        assert_eq!(stream.snapshot(false).counts().total_tokens, Some(15));
        assert_eq!(stream.snapshot(false).coverage(), UsageCoverage::Partial);
        assert_eq!(stream.finish(true).coverage(), UsageCoverage::Complete);
    }
}

#[test]
fn stream_without_usage_stays_unavailable_after_terminal_event() {
    let usage = StreamUsage::new(ProviderUsage::OpenAiChat).finish(true);
    assert_eq!(usage.coverage(), UsageCoverage::Unavailable);
}

#[test]
fn regressing_or_malformed_updates_keep_previous_subtotals_but_not_completeness() {
    for later in [
        json!({"output_tokens":2}),
        json!({"output_tokens":"bad"}),
        json!([]),
    ] {
        let mut stream = StreamUsage::new(ProviderUsage::OpenAiResponses);
        stream.update(&json!({"input_tokens":10,"output_tokens":5,"total_tokens":15}));
        stream.update(&later);
        let usage = stream.finish(true);
        assert_eq!(usage.counts().completion_tokens, Some(5));
        assert_eq!(usage.counts().total_tokens, Some(15));
        assert_eq!(usage.coverage(), UsageCoverage::Partial);
    }
}

#[test]
fn anthropic_missing_categories_keep_lower_bounds_but_not_complete_totals() {
    let usage =
        ProviderUsage::Anthropic.parse(Some(&json!({"input_tokens":10,"output_tokens":5})), true);
    assert_eq!(usage.counts().prompt_tokens, Some(10));
    assert_eq!(usage.counts().total_tokens, Some(15));
    assert_eq!(usage.coverage(), UsageCoverage::Partial);
    assert_eq!(usage.counts().cached_tokens, None);
}

#[test]
fn receipt_roundtrip_preserves_unknown_and_rejects_forged_coverage() {
    for usage in [UsageReceipt::default(), receipt(20, 5)] {
        let encoded = serde_json::to_value(&usage).unwrap();
        assert_eq!(
            serde_json::from_value::<UsageReceipt>(encoded).unwrap(),
            usage
        );
    }
    let mut encoded = serde_json::to_value(UsageReceipt::default()).unwrap();
    encoded["coverage"] = json!("complete");
    assert!(serde_json::from_value::<UsageReceipt>(encoded).is_err());
    let mut encoded = serde_json::to_value(receipt(20, 5)).unwrap();
    encoded["counts"]["total_tokens"] = json!(100);
    assert!(serde_json::from_value::<UsageReceipt>(encoded).is_err());
}

#[test]
fn disjoint_scope_merging_matches_serial_receipt_accounting() {
    let mut left = UsageAggregate::default();
    let mut right = UsageAggregate::default();
    let mut serial = UsageAggregate::default();
    for i in 0..40 {
        let usage = if i % 3 == 0 {
            UsageReceipt::default()
        } else {
            receipt(i, 2)
        };
        serial.add(&usage).unwrap();
        if i % 2 == 0 {
            left.add(&usage).unwrap();
        } else {
            right.add(&usage).unwrap();
        }
    }
    left.merge(&right).unwrap();
    assert_eq!(left, serial);
}

#[test]
fn total_cannot_be_less_than_a_known_component_even_if_other_component_is_missing() {
    let usage = ProviderUsage::OpenAiResponses.parse(
        Some(&json!({
            "input_tokens":10,"total_tokens":5
        })),
        true,
    );
    assert_eq!(usage.counts().total_tokens, None);
    assert_eq!(usage.counts().prompt_tokens, Some(10));
}

#[test]
fn known_optional_subtotals_remain_partial_when_another_request_omits_detail() {
    let mut aggregate = UsageAggregate::default();
    aggregate
        .add(&ProviderUsage::OpenAiResponses.parse(
            Some(&json!({
                "input_tokens":20,"output_tokens":5,"total_tokens":25,
                "output_tokens_details":{"reasoning_tokens":3}
            })),
            true,
        ))
        .unwrap();
    aggregate.add(&receipt(10, 5)).unwrap();
    assert_eq!(aggregate.coverage(), UsageCoverage::Complete);
    assert_eq!(aggregate.reasoning_tokens().known(), Some(3));
    assert!(!aggregate.reasoning_tokens().complete());
    assert_eq!(aggregate.total_tokens().known(), Some(40));
    assert!(aggregate.total_tokens().complete());
}

#[test]
fn cache_categories_cannot_jointly_exceed_the_input_total() {
    let usage = ProviderUsage::OpenAiResponses.parse(
        Some(&json!({
            "input_tokens":20,"output_tokens":5,"total_tokens":25,
            "input_tokens_details":{"cached_tokens":12,"cache_write_tokens":12}
        })),
        true,
    );
    assert_eq!(usage.coverage(), UsageCoverage::Partial);
    assert_eq!(usage.counts().total_tokens, Some(25));
    assert_eq!(usage.counts().cached_tokens, None);
    assert_eq!(usage.counts().cache_write_tokens, None);
}

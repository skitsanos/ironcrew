use super::*;

#[test]
fn encoded_output_budget_counts_json_escaping() {
    let mut bytes = 2;
    reserve_foreach_output("foreach", "\\\"", &mut bytes, 16).unwrap();
    assert_eq!(bytes, serde_json::to_string(&vec!["\\\""]).unwrap().len());

    let cap = bytes + 2;
    let error = reserve_foreach_output("foreach", "too large", &mut bytes, cap).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("IRONCREW_FOREACH_MAX_OUTPUT_BYTES")
    );
}

#[test]
fn repeated_item_expansion_is_bounded_without_splitting_utf8() {
    let template = "${item}".repeat(10_000);
    let replacement = "🦀".repeat(10_000);

    let (expanded, truncated) = replace_bounded(&template, "${item}", &replacement, 128);

    assert!(truncated);
    assert_eq!(expanded.chars().count(), 128);
    assert!(expanded.ends_with(FOREACH_TRUNCATION_MARKER));
    assert!(std::str::from_utf8(expanded.as_bytes()).is_ok());
    assert!(expanded.len() < 1024);
}

#[test]
fn non_string_item_serialization_respects_field_budget() {
    let item = serde_json::json!({"payload": "é".repeat(100_000)});
    let (text, truncated) = bounded_item_text(&item, 96);

    assert!(truncated);
    assert!(text.chars().count() <= 96);
    assert!(text.ends_with(FOREACH_TRUNCATION_MARKER));
    assert!(std::str::from_utf8(text.as_bytes()).is_ok());
}

#[test]
fn item_variable_name_has_a_fixed_hard_limit() {
    let task = Task {
        name: "foreach".into(),
        ..Task::default()
    };
    let oversized = "x".repeat(HARD_FOREACH_ITEM_VAR_BYTES + 1);

    let error = validate_item_var(&task, &oversized).unwrap_err();

    assert!(error.to_string().contains("foreach_as"));
}

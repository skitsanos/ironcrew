use super::operations::{OperationRegistry, ParamType};
use super::policy::AppDbPolicy;

fn policy() -> AppDbPolicy {
    AppDbPolicy::from_values(4, 5_000, 500, 1 << 20, 1 << 20, 64, 64 * 1024)
}

const SAVE: &str = "-- ironcrew:op\n-- params: run_id text, payload json\nINSERT INTO checkpoints (run_id, payload) VALUES ($1, $2)\nON CONFLICT (run_id) DO UPDATE SET payload = EXCLUDED.payload;\n";

#[test]
fn parses_a_valid_operation() {
    let registry =
        OperationRegistry::from_sources(vec![("save".into(), SAVE.into())], &policy()).unwrap();
    let op = registry.get("save").unwrap();
    assert_eq!(op.params.len(), 2);
    assert!(matches!(op.params[1].1, ParamType::Json));
    assert_eq!(op.statements.len(), 1);
    assert_eq!(op.statements[0].bind_count, 2);
    assert!(op.digest.starts_with("sha256:"));
}

#[test]
fn digest_is_stable_and_source_sensitive() {
    let a = OperationRegistry::from_sources(vec![("save".into(), SAVE.into())], &policy()).unwrap();
    let b = OperationRegistry::from_sources(vec![("save".into(), SAVE.into())], &policy()).unwrap();
    assert_eq!(a.get("save").unwrap().digest, b.get("save").unwrap().digest);
    let changed = SAVE.replace("payload", "body");
    let c = OperationRegistry::from_sources(vec![("save".into(), changed)], &policy()).unwrap();
    assert_ne!(a.get("save").unwrap().digest, c.get("save").unwrap().digest);
}

#[test]
fn missing_marker_unknown_type_and_dup_params_are_rejected() {
    let no_marker = "-- params: a text\nSELECT 1;";
    assert!(
        OperationRegistry::from_sources(vec![("x".into(), no_marker.into())], &policy()).is_err()
    );
    let bad_type = "-- ironcrew:op\n-- params: a uuid\nSELECT $1;";
    let err = OperationRegistry::from_sources(vec![("x".into(), bad_type.into())], &policy())
        .unwrap_err()
        .to_string();
    assert!(err.contains("uuid") && err.contains("text"), "{err}");
    let dup = "-- ironcrew:op\n-- params: a text, a text\nSELECT $1;";
    assert!(OperationRegistry::from_sources(vec![("x".into(), dup.into())], &policy()).is_err());
}

#[test]
fn placeholder_beyond_declared_params_is_rejected() {
    let source = "-- ironcrew:op\n-- params: a text\nSELECT $2;";
    let err = OperationRegistry::from_sources(vec![("x".into(), source.into())], &policy())
        .unwrap_err()
        .to_string();
    assert!(err.contains("$2"), "{err}");
}

#[test]
fn zero_param_ops_and_empty_bodies() {
    let ok = "-- ironcrew:op\nSELECT count(*) FROM checkpoints;";
    let registry =
        OperationRegistry::from_sources(vec![("count".into(), ok.into())], &policy()).unwrap();
    assert_eq!(registry.get("count").unwrap().statements[0].bind_count, 0);
    let empty = "-- ironcrew:op\n-- params: a text\n";
    assert!(OperationRegistry::from_sources(vec![("x".into(), empty.into())], &policy()).is_err());
}

#[test]
fn limits_and_names_are_enforced() {
    let small = AppDbPolicy::from_values(4, 5_000, 500, 1 << 20, 1 << 20, 1, 64 * 1024);
    let two = vec![("a".into(), SAVE.into()), ("b".into(), SAVE.into())];
    assert!(OperationRegistry::from_sources(two, &small).is_err());
    assert!(
        OperationRegistry::from_sources(vec![("bad name!".into(), SAVE.into())], &policy())
            .is_err()
    );
    let dup = vec![("a".into(), SAVE.into()), ("a".into(), SAVE.into())];
    assert!(OperationRegistry::from_sources(dup, &policy()).is_err());
}

#[test]
fn definition_lists_ops_sorted_with_digests() {
    let registry = OperationRegistry::from_sources(
        vec![("b".into(), SAVE.into()), ("a".into(), SAVE.into())],
        &policy(),
    )
    .unwrap();
    let definition = registry.definition();
    let ops = definition.as_array().unwrap();
    assert_eq!(ops[0]["name"], "a");
    assert_eq!(ops[1]["name"], "b");
    assert!(ops[0]["digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(ops[0]["params"][0]["type"], "text");
}

#[test]
fn duplicate_params_line_is_rejected_even_when_first_is_empty() {
    let source = "-- ironcrew:op\n-- params:\n-- params: a text\nSELECT $1;\n";
    let err = OperationRegistry::from_sources(vec![("x".into(), source.into())], &policy())
        .unwrap_err()
        .to_string();
    assert!(err.contains("duplicate '-- params:'"), "{err}");
}

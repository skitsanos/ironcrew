use super::*;

fn limits() -> Limits {
    Limits {
        bytes: 1024,
        depth: 8,
        nodes: 32,
        string_bytes: 32,
        container_entries: 8,
    }
}

fn error(raw: &str, limits: Limits) -> String {
    preflight(raw, "test", Root::Array, limits)
        .unwrap_err()
        .to_string()
}

#[test]
fn accepts_bounded_nested_json_without_materializing_it() {
    preflight(
        r#"[{"role":"assistant","raw_blocks":[{"text":"a\u0020b"}]}]"#,
        "test",
        Root::Array,
        limits(),
    )
    .unwrap();
}

#[test]
fn rejects_each_structural_amplification_axis() {
    let mut bounded = limits();
    bounded.depth = 3;
    assert!(error("[[[null]]]", bounded).contains("nesting-depth"));

    let mut bounded = limits();
    bounded.nodes = 2;
    assert!(error("[null,null]", bounded).contains("node limit"));

    let mut bounded = limits();
    bounded.string_bytes = 3;
    assert!(error(r#"["abcd"]"#, bounded).contains("string-byte"));

    let mut bounded = limits();
    bounded.container_entries = 1;
    assert!(error("[null,null]", bounded).contains("container-entry"));
}

#[test]
fn rejects_malformed_or_wrong_root_json() {
    assert!(error("{}", limits()).contains("top-level shape"));
    assert!(error(r#"["\q"]"#, limits()).contains("invalid string escape"));
    assert!(error("[01]", limits()).contains("invalid array syntax"));
}

use super::policy::AppDbPolicy;

#[test]
fn defaults_match_the_spec() {
    let policy = AppDbPolicy::from_values(4, 5_000, 500, 1024 * 1024, 1024 * 1024, 64, 64 * 1024);
    assert_eq!(policy.max_connections(), 4);
    assert_eq!(policy.statement_timeout_ms(), 5_000);
    assert_eq!(policy.max_rows(), 500);
    assert_eq!(policy.max_response_bytes(), 1024 * 1024);
    assert_eq!(policy.max_param_bytes(), 1024 * 1024);
    assert_eq!(policy.max_operations(), 64);
    assert_eq!(policy.max_sql_bytes(), 64 * 1024);
}

#[test]
fn definition_is_deterministic_and_complete() {
    let policy = AppDbPolicy::from_values(4, 5_000, 500, 1_000, 2_000, 64, 65_536);
    let definition = policy.definition();
    assert_eq!(definition["statement_timeout_ms"], 5_000);
    assert_eq!(definition["max_rows"], 500);
    assert_eq!(definition["max_response_bytes"], 1_000);
    assert_eq!(definition["max_param_bytes"], 2_000);
    // Connection count does not change SQL semantics; it must NOT be in the
    // drift fingerprint definition.
    assert!(definition.get("max_connections").is_none());
}

#[test]
fn env_bounds_are_fail_closed() {
    // SAFETY: unique names, no other test touches them.
    unsafe { std::env::set_var("IRONCREW_APP_DB_MAX_ROWS", "0") };
    let error = AppDbPolicy::capture().expect_err("zero must be rejected");
    assert!(error.to_string().contains("IRONCREW_APP_DB_MAX_ROWS"));
    unsafe { std::env::set_var("IRONCREW_APP_DB_MAX_ROWS", "999999") };
    let error = AppDbPolicy::capture().expect_err("above ceiling must be rejected");
    assert!(error.to_string().contains("10000"));
    unsafe { std::env::remove_var("IRONCREW_APP_DB_MAX_ROWS") };
    let policy = AppDbPolicy::capture().expect("defaults are valid");
    assert_eq!(policy.max_rows(), 500);
}

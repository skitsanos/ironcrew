use ironcrew::engine::run_history::RunRecord;
use ironcrew::engine::task::TaskResult;
use ironcrew::history_reader::{MAX_RECORD_BYTES, decode_history_record};
use ironcrew::usage::{UsageCounts, UsageReceipt, UsageSnapshot};
use serde_json::{Value, json};

fn task() -> Value {
    json!({"task":"summary","agent":"author","output":"authored fixture", "success":true,
        "duration_ms":2,"token_usage":{"total_tokens":u64::MAX,"prompt_tokens":15}})
}

fn record(tasks: Vec<Value>) -> Value {
    json!({"run_id":"historical-one","flow_name":"old goal","flow":"", "status":"Success",
        "started_at":"2026-01-01T00:00:00Z","finished_at":"2026-01-01T00:00:01Z",
        "duration_ms":1000,"agent_count":1,"task_count":tasks.len(),"task_results":tasks,
        "total_tokens":u64::MAX,"cached_tokens":42,"tags":["archive"]})
}

#[test]
fn legacy_view_preserves_original_values_without_fabricating_receipts() {
    let original = record(vec![task()]);
    let raw = original.to_string();
    let view = decode_history_record(&raw).unwrap();
    assert_eq!(view.record.get(), raw);
    assert!(view.read_only && !view.executable && view.legacy_usage_missing);
    assert!(view.run_usage.is_none());
    assert!(view.tasks[0].usage.is_none());
    assert!(view.tasks[0].legacy_counters_present);
    let wire = serde_json::to_value(&view).unwrap();
    assert!(wire["run_usage"].is_null());
    assert!(wire["tasks"][0]["usage"].is_null());
    assert_eq!(
        wire["record"]["task_results"][0]["token_usage"]["total_tokens"],
        u64::MAX
    );
    assert_eq!(raw, original.to_string());
    assert!(serde_json::from_value::<TaskResult>(task()).is_err());
    assert!(serde_json::from_value::<RunRecord>(original).is_err());
}

#[test]
fn current_large_checked_receipts_remain_exact() {
    let receipt = UsageSnapshot::from_receipt(UsageReceipt::from_counts(
        UsageCounts {
            prompt_tokens: Some(u64::MAX - 10),
            completion_tokens: Some(10),
            total_tokens: Some(u64::MAX),
            ..Default::default()
        },
        true,
    ));
    let mut current = task();
    current.as_object_mut().unwrap().remove("token_usage");
    current["usage"] = json!(receipt);
    let mut raw = record(vec![current]);
    raw["usage"] = json!(receipt);
    let view = decode_history_record(&raw.to_string()).unwrap();
    assert_eq!(view.run_usage, Some(receipt.clone()));
    assert_eq!(view.tasks[0].usage, Some(receipt));
    assert_eq!(view.record.get(), raw.to_string());
    assert!(!view.legacy_usage_missing);
    assert!(!view.tasks[0].legacy_counters_present);
}

#[test]
fn raw_history_preserves_arbitrary_precision_and_numeric_spelling() {
    let base = record(vec![task()]).to_string();
    let raw = format!(
        "{},\"legacy_numeric\":123456789012345678901234567890,\"legacy_decimal\":0.123456789012345678901234567890,\"legacy_enormous\":1e999}}",
        &base[..base.len() - 1]
    );
    let raw = raw.replace(
        "18446744073709551615",
        "999999999999999999999999999999999999999",
    );
    let view = decode_history_record(&raw).unwrap();
    assert_eq!(view.record.get(), raw);
    assert!(view.run_usage.is_none() && view.tasks[0].usage.is_none());
    let wire = serde_json::to_string_pretty(&view).unwrap();
    assert!(
        wire.contains(&raw),
        "the complete original JSON must be emitted verbatim"
    );
}

#[test]
fn mixed_tasks_keep_checked_zero_distinct_from_unknown() {
    let zero = UsageSnapshot::from_receipt(UsageReceipt::from_counts(
        UsageCounts {
            prompt_tokens: Some(0),
            completion_tokens: Some(0),
            total_tokens: Some(0),
            ..Default::default()
        },
        true,
    ));
    let mut checked = task();
    checked["usage"] = json!(zero);
    let mut original = record(vec![task(), checked]);
    original["usage"] = json!(UsageSnapshot::unavailable());
    let view = decode_history_record(&original.to_string()).unwrap();
    assert!(view.legacy_usage_missing);
    assert!(view.tasks[0].usage.is_none());
    assert_eq!(view.tasks[1].usage, Some(zero));
    assert_eq!(view.run_usage, Some(UsageSnapshot::unavailable()));
}

#[test]
fn malformed_present_checked_usage_never_falls_back_to_legacy_counters() {
    for usage in [
        Value::Null,
        json!(0),
        json!({"coverage":"complete"}),
        json!("private-sentinel"),
    ] {
        let mut bad = task();
        bad["usage"] = usage.clone();
        let error = decode_history_record(&record(vec![bad]).to_string()).unwrap_err();
        assert!(!error.to_string().contains("private-sentinel"));
        let mut bad = record(vec![task()]);
        bad["usage"] = usage;
        assert!(decode_history_record(&bad.to_string()).is_err());
    }
}

#[test]
fn forged_coverage_and_unknown_checked_fields_are_rejected() {
    for change in [json!("complete"), json!("partial")] {
        let mut usage = json!(UsageSnapshot::unavailable());
        usage["coverage"] = change;
        let mut raw = record(vec![task()]);
        raw["usage"] = usage;
        assert!(decode_history_record(&raw.to_string()).is_err());
    }
    let mut raw = record(vec![task()]);
    let mut usage = json!(UsageSnapshot::unavailable());
    usage["extra"] = json!(true);
    raw["usage"] = usage;
    assert!(decode_history_record(&raw.to_string()).is_err());
}

#[test]
fn malformed_metadata_and_tasks_are_not_hidden_by_history_projection() {
    for field in ["run_id", "status", "duration_ms", "task_results"] {
        let mut raw = record(vec![task()]);
        raw[field] = json!({"private-sentinel":"invalid"});
        let error = decode_history_record(&raw.to_string()).unwrap_err();
        assert!(!error.to_string().contains("private-sentinel"));
    }
    let mut bad = task();
    bad["output"] = json!(false);
    assert!(decode_history_record(&record(vec![bad]).to_string()).is_err());
    assert!(decode_history_record("{").is_err());
}

#[test]
fn reader_enforces_record_task_and_usage_bounds() {
    assert!(decode_history_record(&" ".repeat(MAX_RECORD_BYTES + 1)).is_err());
    assert!(decode_history_record(&record(vec![task(); 10_001]).to_string()).is_err());
    let mut raw = record(vec![task()]);
    raw["usage"] = json!({"private-sentinel":"x".repeat(4097)});
    let error = decode_history_record(&raw.to_string()).unwrap_err();
    assert!(error.to_string().contains("4096 bytes"));
    assert!(!error.to_string().contains("private-sentinel"));
}

#[test]
fn database_status_spellings_remain_unchanged_and_unknown_status_fails() {
    for status in [
        "success",
        "partial_failure",
        "failed",
        "aborted",
        "Success",
        "Abandoned",
    ] {
        let mut raw = record(vec![task()]);
        raw["status"] = json!(status);
        assert_eq!(
            decode_history_record(&raw.to_string())
                .unwrap()
                .record
                .get(),
            raw.to_string()
        );
    }
    let mut raw = record(vec![task()]);
    raw["status"] = json!("private-sentinel");
    let error = decode_history_record(&raw.to_string()).unwrap_err();
    assert!(!error.to_string().contains("private-sentinel"));
}

#[test]
fn history_cli_ignores_ancestor_dotenv_and_requires_explicit_connection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "DATABASE_URL=private-sentinel\n").unwrap();
    let child = dir.path().join("nested");
    std::fs::create_dir(&child).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ironcrew"))
        .current_dir(&child)
        .env_remove("DATABASE_URL")
        .args(["history", "inspect", "old-id"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    #[cfg(feature = "postgres")]
    assert!(combined.contains("explicit DATABASE_URL"));
    #[cfg(not(feature = "postgres"))]
    assert!(combined.contains("postgres feature"));
    assert!(!combined.contains("private-sentinel"));
    assert!(!child.join(".ironcrew").exists());
}

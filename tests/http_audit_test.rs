//! End-to-end HTTP integration tests for the audit log.
//!
//! Spins up a real axum server on a random port and exercises:
//!   T5  — POST /flows/{flow}/run produces a flow.run.start audit
//!         event with `source_ip` populated from `ConnectInfo`.
//!   T6  — `X-Audit-Actor` header is captured into the audit event.
//!   T6b — control-character actor is sanitized to None.
//!
//! Uses `reqwest` (already a runtime dep) as the HTTP client so
//! ConnectInfo<SocketAddr> works naturally — same wiring as production.

use std::net::SocketAddr;
use std::sync::Arc;

use ironcrew::api::{AppState, create_router};
use ironcrew::engine::store::create_store;

/// Spin up a real axum server on a random port bound to 127.0.0.1.
/// Returns the bound address; the server task and tempdir are leaked
/// for the lifetime of the test process.
async fn spawn_test_server() -> SocketAddr {
    // SAFETY: tests in this file all want the API token unset. Other
    // tests in the suite that need it set construct their own state.
    // The remove is idempotent so concurrent calls are harmless.
    unsafe { std::env::remove_var("IRONCREW_API_TOKEN") };

    let temp = tempfile::tempdir().unwrap();
    let ironcrew_dir = temp.path().join(".ironcrew");
    std::fs::create_dir_all(&ironcrew_dir).unwrap();

    let store = create_store(ironcrew_dir).await.unwrap();

    // Keep the tempdir alive for the test process so the JSON store
    // can keep writing into it.
    let _ = Box::leak(Box::new(temp));

    let state = Arc::new(AppState {
        flows_dir: std::path::PathBuf::from("examples"),
        active_runs: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        active_conversations: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        max_active_conversations: 100,
        store,
    });

    let app = create_router(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });

    // Brief settle so the listener is accepting before tests fire requests.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    addr
}

#[tokio::test]
async fn test_05_flow_run_produces_audit_event() {
    let addr = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    // Trigger the handler. The flow doesn't exist, so resolution fails
    // and the handler returns 404 — but it still records the audit
    // event on the failure path (handlers.rs run_flow lines ~91-114).
    let _ = client
        .post(format!("{}/flows/audit-smoke/run", base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();

    let resp: serde_json::Value = client
        .get(format!("{}/audit", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let events = resp["events"].as_array().expect("events array in response");
    assert!(
        !events.is_empty(),
        "expected at least one audit event, got: {}",
        resp
    );

    let event = events
        .iter()
        .find(|e| e["action"] == "flow.run.start")
        .unwrap_or_else(|| panic!("flow.run.start event missing in: {}", resp));

    assert_eq!(event["flow_path"], "audit-smoke");
    assert_eq!(event["source_ip"], "127.0.0.1");
    assert_eq!(event["success"], false);
}

#[tokio::test]
async fn test_06_audit_actor_header_captured() {
    let addr = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    let _ = client
        .post(format!("{}/flows/actor-smoke/run", base))
        .header("X-Audit-Actor", "alice@example.com")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();

    let resp: serde_json::Value = client
        .get(format!("{}/audit?actor=alice@example.com", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let events = resp["events"].as_array().expect("events array in response");
    assert!(
        events
            .iter()
            .any(|e| e["actor"] == "alice@example.com" && e["flow_path"] == "actor-smoke"),
        "expected event with actor=alice@example.com, flow_path=actor-smoke, got: {}",
        resp
    );
}

#[tokio::test]
async fn test_06b_actor_with_control_chars_rejected() {
    let addr = spawn_test_server().await;
    let base = format!("http://{}", addr);
    let client = reqwest::Client::new();

    // Build a request manually so we can insert a header value
    // containing a control character. HTTP only allows VCHAR + SP +
    // HTAB in field-values (RFC 7230 §3.2), so most control bytes
    // (e.g. 0x01) are rejected by `HeaderValue::from_bytes`. HTAB
    // (0x09) is permitted on the wire but `char::is_control()` still
    // returns true for it, so `extract_actor` sanitizes it to None —
    // exercising exactly the path we care about.
    let mut req = client
        .post(format!("{}/flows/control-smoke/run", base))
        .json(&serde_json::json!({}))
        .build()
        .unwrap();
    req.headers_mut().insert(
        "X-Audit-Actor",
        reqwest::header::HeaderValue::from_bytes(b"alice\tbob").unwrap(),
    );
    let _ = client.execute(req).await.unwrap();

    let resp: serde_json::Value = client
        .get(format!("{}/audit?flow=control-smoke", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let events = resp["events"].as_array().expect("events array in response");
    let event = events
        .iter()
        .find(|e| e["flow_path"] == "control-smoke")
        .unwrap_or_else(|| panic!("control-smoke event missing in: {}", resp));

    // Actor was sanitized to None. With `skip_serializing_if = "Option::is_none"`
    // the field is absent from the serialized JSON.
    assert!(
        event.get("actor").is_none() || event["actor"].is_null(),
        "expected actor to be absent/null after sanitization, got: {}",
        event
    );
}

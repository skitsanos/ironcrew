//! Poll lock contenders directly: sleeping after spawning a TCP request does
//! not establish that its handler has reached the lock. These tests exercise
//! the real router; socket and backend health coverage stays in the parent.

use std::future::Future;
use std::task::Poll;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use ironcrew::api::{CachedReadiness, create_router};
use ironcrew::engine::audit::AuditFilter;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{TestServer, spawn_server};

// Only a deadlock watchdog, not an HTTP latency requirement. In particular,
// rejecting a busy conversation still includes durable audit filesystem I/O.
const WATCHDOG: Duration = Duration::from_secs(10);

fn request(
    server: &TestServer,
    method: Method,
    path: &str,
    body: Value,
) -> impl Future<Output = Response> + use<> {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(body.to_string()))
        .unwrap();
    let router = create_router(server.state.clone());
    async move { router.oneshot(request).await.unwrap() }
}

async fn json_body(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 8 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn overlapping_conversation_message_fails_fast_instead_of_queueing() {
    let server = spawn_server(2, 1, Duration::from_secs(60)).await;
    let started = request(
        &server,
        Method::POST,
        "/flows/chat/conversations/busy/start",
        json!({ "agent": "tutor" }),
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);

    let handle = server
        .state
        .active_conversations
        .read()
        .await
        .get(&("chat".to_string(), "busy".to_string()))
        .cloned()
        .expect("active conversation handle");
    let active_turn = handle.turn_lock.lock().await;

    let response = tokio::time::timeout(
        WATCHDOG,
        request(
            &server,
            Method::POST,
            "/flows/chat/conversations/busy/messages",
            json!({ "content": "do not queue" }),
        ),
    )
    .await
    .expect("busy response must finish while the active turn is still locked");

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json_body(response).await;
    assert_eq!(
        body["error"],
        "Conversation is busy; retry after the active operation completes"
    );
    let audits = server
        .store
        .list_audit_events(
            &AuditFilter {
                flow_path: Some("chat".into()),
                action: Some("conversation.message".into()),
                success: Some(false),
                ..Default::default()
            },
            10,
            0,
        )
        .await
        .unwrap();
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0].target.as_deref(), Some("busy"));
    assert_eq!(audits[0].status_code, 409);
    assert_eq!(audits[0].metadata, Some(json!({ "idempotent": false })));
    drop(active_turn);
}

#[tokio::test(start_paused = true)]
async fn overlapping_readiness_probes_coalesce_without_a_false_503() {
    let server = spawn_server(1, 4, Duration::from_secs(60)).await;
    let mut cache_guard = server.state.readiness_cache.lock().await;
    let request = request(&server, Method::GET, "/health/ready", Value::Null);
    tokio::pin!(request);

    assert!(
        futures::poll!(&mut request).is_pending(),
        "an overlapping healthy probe must wait for the in-flight check"
    );
    // Model the active probe publishing its completed health check. The
    // contender must reuse this fresh snapshot without another storage check.
    *cache_guard = Some(CachedReadiness {
        checked_at: Instant::now(),
        ready: true,
        component: "storage",
    });
    drop(cache_guard);

    let Poll::Ready(response) = futures::poll!(&mut request) else {
        panic!("the queued probe must consume the completed check on its next poll");
    };
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["status"], "ready");
    assert_eq!(body["component"], "storage");
}

#[tokio::test(start_paused = true)]
async fn queued_readiness_probe_observes_a_drain_transition() {
    let server = spawn_server(1, 4, Duration::from_secs(60)).await;
    let mut cache_guard = server.state.readiness_cache.lock().await;
    let request = request(&server, Method::GET, "/health/ready", Value::Null);
    tokio::pin!(request);

    assert!(
        futures::poll!(&mut request).is_pending(),
        "the probe must be queued behind the active readiness check"
    );
    server.state.lifecycle.begin_fencing();
    *cache_guard = Some(CachedReadiness {
        checked_at: Instant::now(),
        ready: true,
        component: "storage",
    });
    drop(cache_guard);

    let Poll::Ready(response) = futures::poll!(&mut request) else {
        panic!("the queued probe must observe fencing on its next poll");
    };
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await;
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["component"], "lifecycle");
    assert_eq!(body["lifecycle_state"], "fencing");
}

#[tokio::test(start_paused = true)]
async fn overlapping_readiness_probe_fails_closed_when_the_check_stalls() {
    let server = spawn_server(1, 4, Duration::from_secs(60)).await;
    let cache_guard = server.state.readiness_cache.lock().await;
    let request = request(&server, Method::GET, "/health/ready", Value::Null);
    tokio::pin!(request);
    assert!(
        futures::poll!(&mut request).is_pending(),
        "a contended probe must wait before failing closed"
    );

    tokio::time::advance(Duration::from_millis(999)).await;
    assert!(
        futures::poll!(&mut request).is_pending(),
        "the probe must keep coalescing until its one-second deadline"
    );
    // Tokio timers have millisecond granularity. Cross the one-second bound
    // by one tick, without sleeping or giving a stalled handler extra time.
    tokio::time::advance(Duration::from_millis(2)).await;
    let Poll::Ready(response) = futures::poll!(&mut request) else {
        panic!(
            "the probe must fail closed at its one-second deadline while the cache stays locked"
        );
    };
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await;
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["component"], "readiness_check");
    drop(cache_guard);
}

use std::sync::{Arc, Mutex};

use axum::{Router, http::HeaderMap, routing::get};
use futures::FutureExt;

use super::*;

struct ScriptedReplay {
    url: String,
    requests: Arc<Mutex<Vec<HeaderMap>>>,
    task: tokio::task::JoinHandle<()>,
}

impl ScriptedReplay {
    async fn start(failures: usize, failure_body: &'static str) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let app = Router::new().route(
            "/events",
            get(move |headers: HeaderMap| {
                let observed = observed.clone();
                async move {
                    let mut observed = observed.lock().unwrap();
                    observed.push(headers);
                    if observed.len() <= failures {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("cache-control", "no-store")],
                            failure_body,
                        )
                    } else {
                        (StatusCode::OK, [("cache-control", "no-store")], "replayed")
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/events", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url,
            requests,
            task,
        }
    }
}

impl Drop for ScriptedReplay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn client() -> Client {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

#[tokio::test]
async fn reconnect_recovers_on_fifth_attempt_and_preserves_auth_and_cursor() {
    let server = ScriptedReplay::start(4, JOURNAL_UNAVAILABLE_BODY).await;
    let response = get_sse_at_url_when_available(
        &client(),
        &server.url,
        Some(HeaderValue::from_static("run:1")),
        "reconnect regression",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "replayed");
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    for headers in requests.iter() {
        assert_eq!(headers["last-event-id"], "run:1");
        assert_eq!(headers["authorization"], format!("Bearer {API_TOKEN}"));
    }
}

#[tokio::test]
async fn reconnect_stops_after_five_unavailable_responses() {
    let server = ScriptedReplay::start(5, JOURNAL_UNAVAILABLE_BODY).await;
    let outcome = std::panic::AssertUnwindSafe(get_sse_at_url_when_available(
        &client(),
        &server.url,
        None,
        "exhausted reconnect regression",
    ))
    .catch_unwind()
    .await;
    assert!(outcome.is_err(), "persistent failure must fail the probe");
    assert_eq!(server.requests.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn reconnect_rejects_other_unavailable_responses_without_retry() {
    let server = ScriptedReplay::start(1, r#"{"error":"another failure"}"#).await;
    let outcome = std::panic::AssertUnwindSafe(get_sse_at_url_when_available(
        &client(),
        &server.url,
        None,
        "unexpected reconnect regression",
    ))
    .catch_unwind()
    .await;
    assert!(outcome.is_err(), "unexpected 503 must fail the probe");
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

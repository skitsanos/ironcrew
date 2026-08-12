use std::panic::{AssertUnwindSafe, resume_unwind};

use futures::{FutureExt, future::LocalBoxFuture};
use reqwest::StatusCode;

use super::*;

mod ic005;
mod ic006;
mod ic008;
mod ic008_assert;
mod ic008_http;
mod ic008_process;
mod ic008_provider;
mod ic008_sql;
mod ic016;
mod ic016_http;
mod ic016_process;
mod ic016_sql;
mod ic017;
mod ic017_deadline;
mod ic017_http;
mod ic017_support;
mod ic019;
mod ic019_http;
mod ic019_limits;
mod ic019_quota;
mod ic019_rates;
mod ic019_support;
mod ic020;
mod support;

use support::{
    AdvisoryLock, ScopedSnapshot, assert_stale_completion_fenced, snapshot, wait_for_log,
    wait_until_blocked,
};

pub(super) struct ProcessPair {
    pub(super) database_url: String,
    pub(super) prefix: String,
    pub(super) client: Client,
    pub(super) owner_a: ReplicaProcess,
    pub(super) survivor_b: ReplicaProcess,
    pub(super) owner_a_id: String,
    _workspace: tempfile::TempDir,
}

impl ProcessPair {
    async fn start(
        label: &str,
        require_key: bool,
        fixture: &str,
        database_url: String,
        prefix: String,
        unique: &str,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let workspace = tempfile::tempdir().expect("create phase-two process workspace");
        let flow_dir = workspace.path().join(FLOW);
        fs::create_dir_all(&flow_dir).expect("create phase-two flow directory");
        fs::write(flow_dir.join("crew.lua"), fixture).expect("write phase-two flow fixture");

        let owner_a_id = format!("ic-{label}-{}-a", &unique[..8]);
        let survivor_b_id = format!("ic-{label}-{}-b", &unique[..8]);
        let (port_a, port_b) = reserve_two_ports();
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("build phase-two HTTP client");
        let mut owner_a = ReplicaProcess::spawn_with_policy(
            &format!("{label}-a"),
            &owner_a_id,
            port_a,
            workspace.path(),
            &database_url,
            &prefix,
            workspace.path(),
            require_key,
            extra_env,
        );
        let mut survivor_b = ReplicaProcess::spawn_with_policy(
            &format!("{label}-b"),
            &survivor_b_id,
            port_b,
            workspace.path(),
            &database_url,
            &prefix,
            workspace.path(),
            require_key,
            extra_env,
        );
        tokio::join!(
            owner_a.wait_until_ready(&client),
            survivor_b.wait_until_ready(&client)
        );
        assert_ne!(owner_a.id(), survivor_b.id());
        Self {
            database_url,
            prefix,
            client,
            owner_a,
            survivor_b,
            owner_a_id,
            _workspace: workspace,
        }
    }

    fn stop(&mut self) {
        fn stop_one(process: &mut ReplicaProcess) {
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| process.shutdown()));
            if matches!(process.child.try_wait(), Ok(None)) {
                let _ = process.child.kill();
                let _ = process.child.wait();
            }
        }

        stop_one(&mut self.owner_a);
        stop_one(&mut self.survivor_b);
    }
}

pub(super) async fn with_process_pair<F>(label: &str, require_key: bool, fixture: &str, scenario: F)
where
    F: for<'a> FnOnce(&'a mut ProcessPair) -> LocalBoxFuture<'a, ()>,
{
    with_configured_process_pair(label, require_key, fixture, &[], scenario).await;
}

pub(super) async fn with_configured_process_pair<F>(
    label: &str,
    require_key: bool,
    fixture: &str,
    extra_env: &[(&str, &str)],
    scenario: F,
) where
    F: for<'a> FnOnce(&'a mut ProcessPair) -> LocalBoxFuture<'a, ()>,
{
    let Some(database_url) = postgres_url() else {
        eprintln!("SKIP {label}: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("p2_{}_", &unique[..16]);
    reset_schema(&database_url, &prefix).await;
    let startup = AssertUnwindSafe(ProcessPair::start(
        label,
        require_key,
        fixture,
        database_url.clone(),
        prefix.clone(),
        &unique,
        extra_env,
    ))
    .catch_unwind()
    .await;
    let mut pair = match startup {
        Ok(pair) => pair,
        Err(payload) => {
            // Preserve the setup failure if cleanup also encounters a broken
            // database; the original panic carries the useful process logs.
            let _ = AssertUnwindSafe(reset_schema(&database_url, &prefix))
                .catch_unwind()
                .await;
            resume_unwind(payload);
        }
    };
    let outcome = AssertUnwindSafe(scenario(&mut pair)).catch_unwind().await;
    pair.stop();
    let cleanup = AssertUnwindSafe(reset_schema(&pair.database_url, &pair.prefix))
        .catch_unwind()
        .await;
    match (outcome, cleanup) {
        (Err(payload), _) => resume_unwind(payload),
        (Ok(()), Err(payload)) => resume_unwind(payload),
        (Ok(()), Ok(())) => {}
    }
}

pub(super) async fn start_keyed_run(pair: &ProcessPair, key: &str) -> serde_json::Value {
    let response = authenticated(
        pair.client
            .post(format!("{}/flows/{FLOW}/run", pair.owner_a.base_url)),
    )
    .header("Idempotency-Key", key)
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("start phase-two keyed run");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("Idempotency-Replayed").is_none());
    response.json().await.expect("parse keyed run acceptance")
}

pub(super) async fn wait_for_shared_question(
    pair: &ProcessPair,
    run_id: &str,
    prompt: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let response = authenticated(pair.client.get(format!(
            "{}/flows/{FLOW}/questions/{run_id}",
            pair.survivor_b.base_url
        )))
        .send()
        .await
        .expect("poll phase-two question");
        if response.status() == StatusCode::OK {
            let body: serde_json::Value = response.json().await.expect("parse question response");
            let found = body["questions"]
                .as_array()
                .is_some_and(|questions| questions.iter().any(|item| item["prompt"] == prompt));
            if found {
                assert_eq!(body["status"], "waiting_for_input");
                assert_eq!(body["owner_instance_id"], pair.owner_a_id);
                assert_eq!(body["control_scope"], "shared_store");
                return body["questions"]
                    .as_array()
                    .and_then(|questions| questions.iter().find(|item| item["prompt"] == prompt))
                    .expect("located shared question")
                    .clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("shared question {prompt:?} did not appear");
}

pub(super) async fn read_run(pair: &ProcessPair, run_id: &str) -> serde_json::Value {
    authenticated(pair.client.get(format!(
        "{}/flows/{FLOW}/runs/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("read phase-two run")
    .error_for_status()
    .expect("phase-two run response")
    .json()
    .await
    .expect("parse phase-two run")
}

pub(super) async fn wait_for_status(
    pair: &ProcessPair,
    run_id: &str,
    expected: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let record = read_run(pair, run_id).await;
        if record["status"] == expected {
            return record;
        }
        if matches!(
            record["status"].as_str(),
            Some("Success" | "Failed" | "Aborted" | "Abandoned" | "TimedOut" | "PartialFailure")
        ) {
            panic!(
                "run terminalized as {} instead of {expected}",
                record["status"]
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("run did not reach {expected}");
}

pub(super) async fn wait_for_next_lease(
    pair: &ProcessPair,
    run_id: &str,
) -> chrono::DateTime<chrono::Utc> {
    let initial = read_run(pair, run_id).await;
    assert_eq!(initial["status"], "WaitingForInput");
    let original = initial["lease_expires_at"]
        .as_str()
        .expect("initial lease")
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("parse initial lease");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let observed = read_run(pair, run_id).await;
        assert_eq!(observed["status"], "WaitingForInput");
        if let Some(renewed) = observed["lease_expires_at"]
            .as_str()
            .and_then(|value| value.parse::<chrono::DateTime<chrono::Utc>>().ok())
            && renewed > original
        {
            return renewed;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("owner lease did not renew");
}

pub(super) async fn abort_run(pair: &ProcessPair, run_id: &str) -> serde_json::Value {
    let response = authenticated(pair.client.post(format!(
        "{}/flows/{FLOW}/abort/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("request phase-two cancellation");
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.expect("parse cancellation response")
}

pub(super) fn assert_sigkill(process: &mut ReplicaProcess) {
    let pid = process.id();
    let status = process.sigkill();
    assert!(
        !status.success(),
        "SIGKILLed owner PID {pid} exited cleanly"
    );
    assert_eq!(
        status.signal(),
        Some(nix::sys::signal::Signal::SIGKILL as i32)
    );
}

pub(super) async fn assert_ready(pair: &ProcessPair) {
    let response = pair
        .client
        .get(format!("{}/health/ready", pair.survivor_b.base_url))
        .send()
        .await
        .expect("read survivor readiness");
    assert_eq!(response.status(), StatusCode::OK);
}

pub(super) async fn wait_ready(pair: &mut ProcessPair) {
    pair.survivor_b.wait_until_ready(&pair.client).await;
}

pub(super) async fn assert_replay(
    pair: &ProcessPair,
    key: &str,
    run_id: &str,
    expected: &serde_json::Value,
) {
    let response = replay_empty_keyed_run(&pair.client, &pair.survivor_b.base_url, key).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("Idempotency-Replayed")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    let body: serde_json::Value = response.json().await.expect("parse phase-two replay");
    assert_eq!(body["run_id"], run_id);
    assert_eq!(body["owner_instance_id"], pair.owner_a_id);
    assert_eq!(body["control_scope"], "process");
    assert_eq!(&body, expected);
}

pub(super) async fn synthesized_abandoned_frame(pair: &ProcessPair, run_id: &str) -> String {
    let response = authenticated(pair.client.get(format!(
        "{}/flows/{FLOW}/events/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("read reconciled SSE fallback");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.expect("read reconciled SSE body");
    let frames: Vec<_> = body
        .split("\n\n")
        .filter(|frame| frame.lines().any(|line| line == "event: run_complete"))
        .collect();
    assert_eq!(
        frames.len(),
        1,
        "expected one synthesized completion: {body}"
    );
    let frame = frames[0];
    assert!(frame.lines().all(|line| !line.starts_with("id:")));
    let payload = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("synthesized completion payload");
    let payload: serde_json::Value =
        serde_json::from_str(payload).expect("parse synthesized completion");
    assert_eq!(payload["event"], "run_complete");
    assert_eq!(payload["data"]["run_id"], run_id);
    assert_eq!(payload["data"]["status"], "abandoned");
    assert_eq!(payload["data"]["journal_complete"], false);
    assert_eq!(payload["data"]["synthesized_from_run_record"], true);
    frame.to_string()
}

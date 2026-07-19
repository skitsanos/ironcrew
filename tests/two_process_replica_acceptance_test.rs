//! Genuine OS-process acceptance coverage for PostgreSQL replica controls.
//!
//! Unlike the router-level multi-replica tests, this test launches two
//! independent `ironcrew serve` executables with distinct PIDs, ports, and
//! instance ids. It is intentionally gated by `IRONCREW_TEST_PG_URL` so the
//! normal test suite remains hermetic.
#![cfg(feature = "postgres")]

use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder, Response, StatusCode};

const FLOW: &str = "replica-acceptance";
const API_TOKEN: &str = "two-process-acceptance-token-32-bytes";
const IDEMPOTENCY_KEY: &str = "two-process-replica-acceptance-key-0001";
const CANCELLATION_IDEMPOTENCY_KEY: &str = "two-process-replica-cancellation-key-0002";
const KEYRING_JSON: &str =
    r#"{"acceptance-key-v1":"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="}"#;
const ACTIVE_KEY_ID: &str = "acceptance-key-v1";
const FIRST_PROMPT: &str = "Approve the genuine two-process handoff?";
const SECOND_PROMPT: &str = "Finish the genuine two-process acceptance run?";
const FIRST_ANSWER: &str = "approved-by-replica-b";
const SECOND_ANSWER: &str = "finished-by-replica-b";

fn postgres_url() -> Option<String> {
    std::env::var("IRONCREW_TEST_PG_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn authenticated(request: RequestBuilder) -> RequestBuilder {
    request.bearer_auth(API_TOKEN)
}

fn reserve_two_ports() -> (u16, u16) {
    let first = TcpListener::bind(("127.0.0.1", 0)).expect("reserve first replica port");
    let second = TcpListener::bind(("127.0.0.1", 0)).expect("reserve second replica port");
    let first_port = first.local_addr().expect("first local address").port();
    let second_port = second.local_addr().expect("second local address").port();
    assert_ne!(first_port, second_port);
    (first_port, second_port)
}

async fn reset_schema(database_url: &str, prefix: &str) {
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .expect("connect to PostgreSQL for acceptance-test cleanup");
    for suffix in [
        "human_inputs",
        "run_events",
        "run_event_state",
        "run_event_usage",
        "runs",
        "conversations",
        "dialogs",
        "audit_events",
        "idempotency",
        "idempotency_accounting",
    ] {
        let statement = format!("DROP TABLE IF EXISTS {prefix}{suffix} CASCADE");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .expect("drop acceptance-test table");
    }
    for suffix in ["idempotency_acct_fn", "run_events_acct_fn"] {
        let statement = format!("DROP FUNCTION IF EXISTS {prefix}{suffix}() CASCADE");
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .expect("drop acceptance-test accounting function");
    }
    pool.close().await;
}

struct ReplicaProcess {
    name: String,
    base_url: String,
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    diagnostic_redactions: Vec<String>,
}

impl ReplicaProcess {
    fn spawn(
        name: &str,
        instance_id: &str,
        port: u16,
        flows_dir: &Path,
        database_url: &str,
        prefix: &str,
        log_dir: &Path,
    ) -> Self {
        let stdout_path = log_dir.join(format!("{name}.stdout.log"));
        let stderr_path = log_dir.join(format!("{name}.stderr.log"));
        let stdout = File::create(&stdout_path).expect("create replica stdout log");
        let stderr = File::create(&stderr_path).expect("create replica stderr log");

        let mut command = Command::new(env!("CARGO_BIN_EXE_ironcrew"));
        command
            .env_clear()
            .current_dir(flows_dir)
            .args([
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--flows-dir",
            ])
            .arg(flows_dir)
            .env("IRONCREW_STORE", "postgres")
            .env("DATABASE_URL", database_url)
            .env("IRONCREW_PG_TABLE_PREFIX", prefix)
            .env("IRONCREW_INSTANCE_ID", instance_id)
            .env("IRONCREW_RUN_LEASE_TTL_SECONDS", "6")
            .env("IRONCREW_DB_POOL_SIZE", "4")
            .env("IRONCREW_DB_CONNECT_RETRIES", "1")
            .env("IRONCREW_DB_CONNECT_BACKOFF_MS", "50")
            .env("IRONCREW_DB_CONNECT_TIMEOUT_SECS", "5")
            .env("IRONCREW_HITL_ENCRYPTION_KEYS", KEYRING_JSON)
            .env("IRONCREW_HITL_ACTIVE_KEY_ID", ACTIVE_KEY_ID)
            .env("IRONCREW_HITL_POLL_INTERVAL_MS", "500")
            .env("IRONCREW_HITL_READ_TIMEOUT_MS", "2000")
            .env("IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS", "100")
            .env("IRONCREW_EVENT_JOURNAL_READ_TIMEOUT_MS", "2000")
            .env("IRONCREW_API_TOKEN", API_TOKEN)
            .env("IRONCREW_API_PRINCIPAL", "acceptance-client")
            .env("IRONCREW_REQUIRE_IDEMPOTENCY_KEY", "true")
            .env("IRONCREW_MAX_ACTIVE_RUNS", "2")
            .env("IRONCREW_MAX_SSE_CONNECTIONS", "4")
            .env("IRONCREW_MAX_RUN_LIFETIME", "60")
            .env("IRONCREW_SHUTDOWN_TIMEOUT_SECS", "3")
            .env("IRONCREW_SHUTDOWN_DRAIN_MS", "0")
            .env("RUST_LOG", "ironcrew=info")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));

        // Windows process creation and temporary-file resolution rely on
        // these host variables. The absolute IronCrew binary path means PATH
        // is not otherwise needed, but retaining it helps platform tooling.
        for variable in ["PATH", "SystemRoot", "WINDIR", "TMPDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(variable) {
                command.env(variable, value);
            }
        }

        let child = command.spawn().expect("spawn ironcrew serve replica");
        let mut diagnostic_redactions = vec![
            database_url.to_string(),
            API_TOKEN.to_string(),
            KEYRING_JSON.to_string(),
            "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=".to_string(),
        ];
        if let Ok(parsed) = url::Url::parse(database_url)
            && let Some(password) = parsed.password()
            && !password.is_empty()
        {
            diagnostic_redactions.push(password.to_string());
        }
        Self {
            name: name.to_string(),
            base_url: format!("http://127.0.0.1:{port}"),
            child,
            stdout_path,
            stderr_path,
            diagnostic_redactions,
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn logs(&self) -> String {
        let mut logs = format!(
            "stdout:\n{}\nstderr:\n{}",
            fs::read_to_string(&self.stdout_path).unwrap_or_else(|error| error.to_string()),
            fs::read_to_string(&self.stderr_path).unwrap_or_else(|error| error.to_string())
        );
        for secret in &self.diagnostic_redactions {
            logs = logs.replace(secret, "[REDACTED]");
        }
        logs
    }

    async fn wait_until_ready(&mut self, client: &Client) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last_observation = String::from("no HTTP response");
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("inspect replica process") {
                panic!(
                    "{} exited before readiness with {status}\n{}",
                    self.name,
                    self.logs()
                );
            }
            match client
                .get(format!("{}/health/ready", self.base_url))
                .send()
                .await
            {
                Ok(response) if response.status() == StatusCode::OK => return,
                Ok(response) => {
                    last_observation = format!("HTTP {}", response.status());
                }
                Err(error) => last_observation = error.to_string(),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "{} did not become ready ({last_observation})\n{}",
            self.name,
            self.logs()
        );
    }

    fn shutdown(&mut self) -> ExitStatus {
        if let Some(status) = self
            .child
            .try_wait()
            .expect("inspect replica before shutdown")
        {
            return status;
        }

        #[cfg(unix)]
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.child.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .expect("send SIGTERM to replica");

        #[cfg(not(unix))]
        self.child.kill().expect("terminate replica");

        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("wait for replica shutdown") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        self.child.wait().expect("reap timed-out replica")
    }
}

impl Drop for ReplicaProcess {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

async fn wait_for_question(
    client: &Client,
    base_url: &str,
    run_id: &str,
    expected_prompt: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_observation = String::from("no response");
    while Instant::now() < deadline {
        let response =
            authenticated(client.get(format!("{base_url}/flows/{FLOW}/questions/{run_id}")))
                .send()
                .await
                .expect("poll peer replica for pending question");
        let status = response.status();
        let body = response.text().await.expect("read question response");
        last_observation = format!("HTTP {status}: {body}");
        if status == StatusCode::OK {
            let json: serde_json::Value =
                serde_json::from_str(&body).expect("parse question response");
            if let Some(question) = json["questions"].as_array().and_then(|questions| {
                questions
                    .iter()
                    .find(|question| question["prompt"] == expected_prompt)
            }) {
                assert_eq!(json["status"], "waiting_for_input");
                assert_eq!(json["owner_instance_id"], "acceptance-replica-a");
                assert_eq!(json["control_scope"], "shared_store");
                return question.clone();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("question {expected_prompt:?} did not appear through replica B; {last_observation}");
}

async fn read_sse_until(response: &mut Response, marker: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut body = String::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .expect("read replica SSE chunk")
                .expect("replica SSE ended before expected event");
            body.push_str(&String::from_utf8_lossy(&chunk));
            if body.contains(marker) {
                return body;
            }
        }
    })
    .await
    .expect("timed out waiting for replica SSE event")
}

fn last_sse_id(body: &str) -> String {
    body.lines()
        .filter_map(|line| line.strip_prefix("id:").map(str::trim))
        .rfind(|value| !value.is_empty())
        .expect("SSE event id")
        .to_string()
}

fn cursor_sequence(cursor: &str, run_id: &str) -> u64 {
    cursor
        .strip_prefix(run_id)
        .and_then(|suffix| suffix.strip_prefix(':'))
        .and_then(|sequence| sequence.parse().ok())
        .expect("run-scoped SSE cursor")
}

async fn answer_question(
    client: &Client,
    base_url: &str,
    run_id: &str,
    question_id: &str,
    answer: &str,
) -> (StatusCode, String) {
    let response = authenticated(client.post(format!("{base_url}/flows/{FLOW}/answer/{run_id}")))
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": answer,
        }))
        .send()
        .await
        .expect("answer question through peer replica");
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let status = response.status();
    let body = response.text().await.expect("read answer response");
    (status, body)
}

async fn wait_for_terminal(
    client: &Client,
    base_url: &str,
    run_id: &str,
    expected_status: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_observation = String::from("no response");
    while Instant::now() < deadline {
        let response = authenticated(client.get(format!("{base_url}/flows/{FLOW}/runs/{run_id}")))
            .send()
            .await
            .expect("poll terminal run through peer replica");
        let status = response.status();
        let body = response.text().await.expect("read run response");
        last_observation = format!("HTTP {status}: {body}");
        if status == StatusCode::OK {
            let json: serde_json::Value =
                serde_json::from_str(&body).expect("parse terminal run response");
            match json["status"].as_str() {
                Some(status) if status == expected_status => return json,
                Some(
                    "Success" | "Failed" | "Aborted" | "Abandoned" | "TimedOut" | "PartialFailure",
                ) => {
                    panic!("run reached unexpected terminal state: {body}")
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("run did not terminalize through replica B; {last_observation}");
}

async fn abort_through_peer(
    client: &Client,
    base_url: &str,
    run_id: &str,
) -> (StatusCode, serde_json::Value) {
    let response = authenticated(client.post(format!("{base_url}/flows/{FLOW}/abort/{run_id}")))
        .send()
        .await
        .expect("abort run through peer replica");
    let status = response.status();
    let body = response.text().await.expect("read abort response");
    let json = serde_json::from_str(&body).expect("parse abort response");
    (status, json)
}

#[tokio::test]
async fn genuine_two_process_postgres_replica_acceptance() {
    let Some(database_url) = postgres_url() else {
        eprintln!(
            "SKIP genuine_two_process_postgres_replica_acceptance: \
             IRONCREW_TEST_PG_URL unset"
        );
        return;
    };

    let unique = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("proc_{}_", &unique[..16]);
    reset_schema(&database_url, &prefix).await;

    let temp = tempfile::tempdir().expect("create acceptance-test workspace");
    let flow_dir = temp.path().join(FLOW);
    fs::create_dir_all(&flow_dir).expect("create acceptance flow directory");
    fs::write(
        flow_dir.join("crew.lua"),
        include_str!("fixtures/two_process_replica/crew.lua"),
    )
    .expect("write acceptance flow fixture");

    let (port_a, port_b) = reserve_two_ports();
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("build acceptance HTTP client");
    let mut replica_a = ReplicaProcess::spawn(
        "replica-a",
        "acceptance-replica-a",
        port_a,
        temp.path(),
        &database_url,
        &prefix,
        temp.path(),
    );
    let mut replica_b = ReplicaProcess::spawn(
        "replica-b",
        "acceptance-replica-b",
        port_b,
        temp.path(),
        &database_url,
        &prefix,
        temp.path(),
    );
    tokio::join!(
        replica_a.wait_until_ready(&client),
        replica_b.wait_until_ready(&client)
    );

    assert_ne!(
        replica_a.id(),
        replica_b.id(),
        "replicas must be distinct PIDs"
    );

    for replica in [&replica_a, &replica_b] {
        let health: serde_json::Value = client
            .get(format!("{}/health", replica.base_url))
            .send()
            .await
            .expect("query replica health")
            .error_for_status()
            .expect("healthy replica")
            .json()
            .await
            .expect("parse replica health");
        assert_eq!(health["status"], "ok");
        assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));

        let capabilities: serde_json::Value =
            authenticated(client.get(format!("{}/capabilities", replica.base_url)))
                .send()
                .await
                .expect("query replica capabilities")
                .error_for_status()
                .expect("replica capabilities")
                .json()
                .await
                .expect("parse replica capabilities");
        assert_eq!(
            capabilities["live_control"]["human_input"],
            "shared_store_for_keyed_runs"
        );
        assert_eq!(capabilities["live_control"]["sse_replay"], "shared_store");
        assert_eq!(capabilities["multi_replica_control"], false);
    }

    let capabilities_a: serde_json::Value =
        authenticated(client.get(format!("{}/capabilities", replica_a.base_url)))
            .send()
            .await
            .expect("query replica A identity")
            .json()
            .await
            .expect("parse replica A identity");
    let capabilities_b: serde_json::Value =
        authenticated(client.get(format!("{}/capabilities", replica_b.base_url)))
            .send()
            .await
            .expect("query replica B identity")
            .json()
            .await
            .expect("parse replica B identity");
    assert_eq!(capabilities_a["instance_id"], "acceptance-replica-a");
    assert_eq!(capabilities_b["instance_id"], "acceptance-replica-b");

    let started = authenticated(client.post(format!("{}/flows/{FLOW}/run", replica_a.base_url)))
        .header("Idempotency-Key", IDEMPOTENCY_KEY)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("start keyed run on replica A");
    assert_eq!(started.status(), StatusCode::OK);
    let started_body: serde_json::Value = started.json().await.expect("parse run acceptance");
    let run_id = started_body["run_id"]
        .as_str()
        .expect("accepted run id")
        .to_string();
    assert_eq!(started_body["owner_instance_id"], "acceptance-replica-a");

    let replayed = authenticated(client.post(format!("{}/flows/{FLOW}/run", replica_b.base_url)))
        .header("Idempotency-Key", IDEMPOTENCY_KEY)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("replay keyed acceptance through replica B");
    assert_eq!(replayed.status(), StatusCode::OK);
    assert_eq!(
        replayed
            .headers()
            .get("Idempotency-Replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let replayed_body: serde_json::Value = replayed.json().await.expect("parse replayed run");
    assert_eq!(replayed_body["run_id"], run_id);
    assert_eq!(replayed_body["owner_instance_id"], "acceptance-replica-a");

    let conflicting_reuse =
        authenticated(client.post(format!("{}/flows/{FLOW}/run", replica_b.base_url)))
            .header("Idempotency-Key", IDEMPOTENCY_KEY)
            .json(&serde_json::json!({"different": true}))
            .send()
            .await
            .expect("reuse keyed acceptance with a different request through replica B");
    assert_eq!(conflicting_reuse.status(), StatusCode::CONFLICT);
    let conflicting_reuse = conflicting_reuse
        .text()
        .await
        .expect("read conflicting idempotency response");
    assert!(conflicting_reuse.contains("already used for a different request"));
    assert!(!conflicting_reuse.contains(IDEMPOTENCY_KEY));

    let first_question =
        wait_for_question(&client, &replica_b.base_url, &run_id, FIRST_PROMPT).await;
    let first_question_id = first_question["question_id"]
        .as_str()
        .expect("first question id")
        .to_string();

    let mut initial_sse = authenticated(client.get(format!(
        "{}/flows/{FLOW}/events/{run_id}",
        replica_b.base_url
    )))
    .send()
    .await
    .expect("observe initial run events through replica B");
    assert_eq!(initial_sse.status(), StatusCode::OK);
    let initial_events = read_sse_until(&mut initial_sse, "event: human_input_requested").await;
    assert!(!initial_events.contains(FIRST_PROMPT));
    assert!(initial_events.contains("omitted_from_event_journal"));
    let first_cursor = last_sse_id(&initial_events);
    let first_sequence = cursor_sequence(&first_cursor, &run_id);
    drop(initial_sse);

    let (answer_status, answer_body) = answer_question(
        &client,
        &replica_b.base_url,
        &run_id,
        &first_question_id,
        FIRST_ANSWER,
    )
    .await;
    assert_eq!(answer_status, StatusCode::ACCEPTED, "{answer_body}");
    assert!(!answer_body.contains(FIRST_ANSWER));

    let (duplicate_status, duplicate_body) = answer_question(
        &client,
        &replica_b.base_url,
        &run_id,
        &first_question_id,
        "replacement-answer-must-not-win",
    )
    .await;
    assert_eq!(duplicate_status, StatusCode::NOT_FOUND, "{duplicate_body}");
    assert!(!duplicate_body.contains(FIRST_ANSWER));
    assert!(!duplicate_body.contains("replacement-answer-must-not-win"));

    let second_question =
        wait_for_question(&client, &replica_b.base_url, &run_id, SECOND_PROMPT).await;
    let second_question_id = second_question["question_id"]
        .as_str()
        .expect("second question id")
        .to_string();
    assert_ne!(first_question_id, second_question_id);

    let mut resumed_sse = authenticated(client.get(format!(
        "{}/flows/{FLOW}/events/{run_id}",
        replica_b.base_url
    )))
    .header("Last-Event-ID", &first_cursor)
    .send()
    .await
    .expect("resume run events on replica B");
    assert_eq!(resumed_sse.status(), StatusCode::OK);
    let resumed_events = read_sse_until(&mut resumed_sse, "event: human_input_requested").await;
    assert!(resumed_events.contains("event: human_input_received"));
    assert!(!resumed_events.contains(SECOND_PROMPT));
    let second_cursor = last_sse_id(&resumed_events);
    assert!(cursor_sequence(&second_cursor, &run_id) > first_sequence);
    drop(resumed_sse);

    let (finish_status, finish_body) = answer_question(
        &client,
        &replica_b.base_url,
        &run_id,
        &second_question_id,
        SECOND_ANSWER,
    )
    .await;
    assert_eq!(finish_status, StatusCode::ACCEPTED, "{finish_body}");
    assert!(!finish_body.contains(SECOND_ANSWER));

    let terminal = wait_for_terminal(&client, &replica_b.base_url, &run_id, "Success").await;
    assert_eq!(terminal["status"], "Success");
    assert_eq!(terminal["owner_instance_id"], "acceptance-replica-a");

    let terminal_sse = authenticated(client.get(format!(
        "{}/flows/{FLOW}/events/{run_id}",
        replica_b.base_url
    )))
    .header("Last-Event-ID", &second_cursor)
    .send()
    .await
    .expect("observe terminal replay through replica B");
    assert_eq!(terminal_sse.status(), StatusCode::OK);
    let terminal_events = terminal_sse
        .text()
        .await
        .expect("read terminal event replay");
    assert!(terminal_events.contains("event: human_input_received"));
    assert!(terminal_events.contains("event: run_complete"));
    assert!(terminal_events.contains("\"status\":\"success\""));
    assert!(!terminal_events.contains("event: human_input_requested"));
    assert!(!terminal_events.contains(FIRST_ANSWER));
    assert!(!terminal_events.contains(SECOND_ANSWER));

    // Start a second keyed run in process A and race two durable cancellation
    // requests through process B while A is suspended. The PostgreSQL fence
    // must serialize them: exactly one request creates the cancellation and
    // the repeat observes `already_requested=true` without changing owner.
    let cancellation_started =
        authenticated(client.post(format!("{}/flows/{FLOW}/run", replica_a.base_url)))
            .header("Idempotency-Key", CANCELLATION_IDEMPOTENCY_KEY)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("start cancellation run on replica A");
    assert_eq!(cancellation_started.status(), StatusCode::OK);
    let cancellation_started: serde_json::Value = cancellation_started
        .json()
        .await
        .expect("parse cancellation run acceptance");
    let cancellation_run_id = cancellation_started["run_id"]
        .as_str()
        .expect("cancellation run id")
        .to_string();
    assert_eq!(
        cancellation_started["owner_instance_id"],
        "acceptance-replica-a"
    );

    wait_for_question(
        &client,
        &replica_b.base_url,
        &cancellation_run_id,
        FIRST_PROMPT,
    )
    .await;
    let mut cancellation_sse = authenticated(client.get(format!(
        "{}/flows/{FLOW}/events/{cancellation_run_id}",
        replica_b.base_url
    )))
    .send()
    .await
    .expect("observe cancellation run through replica B");
    assert_eq!(cancellation_sse.status(), StatusCode::OK);
    let cancellation_initial =
        read_sse_until(&mut cancellation_sse, "event: human_input_requested").await;
    let cancellation_cursor = last_sse_id(&cancellation_initial);
    drop(cancellation_sse);

    let ((abort_one_status, abort_one), (abort_two_status, abort_two)) = tokio::join!(
        abort_through_peer(&client, &replica_b.base_url, &cancellation_run_id),
        abort_through_peer(&client, &replica_b.base_url, &cancellation_run_id)
    );
    assert_eq!(abort_one_status, StatusCode::OK, "{abort_one}");
    assert_eq!(abort_two_status, StatusCode::OK, "{abort_two}");
    for abort in [&abort_one, &abort_two] {
        assert_eq!(abort["status"], "cancellation_requested");
        assert_eq!(abort["owner_instance_id"], "acceptance-replica-a");
        assert_eq!(abort["control_scope"], "shared_store");
    }
    let mut repeated_flags = [
        abort_one["already_requested"]
            .as_bool()
            .expect("first repeat flag"),
        abort_two["already_requested"]
            .as_bool()
            .expect("second repeat flag"),
    ];
    repeated_flags.sort_unstable();
    assert_eq!(repeated_flags, [false, true]);

    // Keep repeating the request while process A observes the cancellation.
    // Depending on the exact transaction boundary, the first request racing
    // terminalization may either see the terminal winner directly or the
    // flow-scoped 404 used after completion. Both are safe, non-mutating
    // terminal-boundary outcomes; no request may create a second winner.
    let race_deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_terminal_boundary = false;
    while Instant::now() < race_deadline {
        let (status, body) =
            abort_through_peer(&client, &replica_b.base_url, &cancellation_run_id).await;
        match status {
            StatusCode::OK if body["terminal"] == true => {
                assert_eq!(body["status"], "aborted");
                saw_terminal_boundary = true;
                break;
            }
            StatusCode::OK => {
                assert_eq!(body["status"], "cancellation_requested");
                assert_eq!(body["already_requested"], true);
                assert_eq!(body["owner_instance_id"], "acceptance-replica-a");
            }
            StatusCode::NOT_FOUND => {
                assert_eq!(
                    body["error"],
                    format!("Run '{cancellation_run_id}' not found or already completed")
                );
                saw_terminal_boundary = true;
                break;
            }
            other => panic!("unexpected cancellation-race response {other}: {body}"),
        }
        // Stay within the production-default control admission bucket while
        // still sampling both sides of the owner's two-second lease heartbeat.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        saw_terminal_boundary,
        "repeated cancellation did not cross the terminal boundary"
    );

    let cancelled = wait_for_terminal(
        &client,
        &replica_b.base_url,
        &cancellation_run_id,
        "Aborted",
    )
    .await;
    assert_eq!(cancelled["owner_instance_id"], "acceptance-replica-a");

    let cancellation_terminal = authenticated(client.get(format!(
        "{}/flows/{FLOW}/events/{cancellation_run_id}",
        replica_b.base_url
    )))
    .header("Last-Event-ID", cancellation_cursor)
    .send()
    .await
    .expect("resume cancelled run events through replica B");
    assert_eq!(cancellation_terminal.status(), StatusCode::OK);
    let cancellation_terminal = cancellation_terminal
        .text()
        .await
        .expect("read cancelled run event replay");
    assert!(cancellation_terminal.contains("event: run_complete"));
    assert!(cancellation_terminal.contains("\"status\":\"aborted\""));

    let status_b = replica_b.shutdown();
    let status_a = replica_a.shutdown();
    if cfg!(unix) {
        assert!(
            status_b.success(),
            "replica B shutdown failed: {}",
            replica_b.logs()
        );
        assert!(
            status_a.success(),
            "replica A shutdown failed: {}",
            replica_a.logs()
        );
    }

    reset_schema(&database_url, &prefix).await;
}

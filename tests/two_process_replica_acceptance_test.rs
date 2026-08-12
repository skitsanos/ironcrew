//! Genuine OS-process acceptance coverage for PostgreSQL replica controls.
//!
//! Unlike the router-level multi-replica tests, this test launches two
//! independent `ironcrew serve` executables with distinct PIDs, ports, and
//! instance ids. It is intentionally gated by `IRONCREW_TEST_PG_URL` so the
//! normal test suite remains hermetic.
#![cfg(feature = "postgres")]

use std::fs::{self, File};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use ironcrew::engine::postgres_store::PostgresStore;
use ironcrew::engine::store::RunLeaseConfig;
use reqwest::{Client, RequestBuilder, Response, StatusCode};

#[cfg(unix)]
#[path = "two_process_replica_acceptance/phase2/mod.rs"]
mod phase2;

const FLOW: &str = "replica-acceptance";
const API_TOKEN: &str = "two-process-acceptance-token-32-bytes";
const IDEMPOTENCY_KEY: &str = "two-process-replica-acceptance-key-0001";
const CANCELLATION_IDEMPOTENCY_KEY: &str = "two-process-replica-cancellation-key-0002";
const OWNER_DEATH_IDEMPOTENCY_KEY: &str = "two-process-owner-death-key-0003";
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
        Self::spawn_with_policy(
            name,
            instance_id,
            port,
            flows_dir,
            database_url,
            prefix,
            log_dir,
            true,
            &[],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_with_policy(
        name: &str,
        instance_id: &str,
        port: u16,
        flows_dir: &Path,
        database_url: &str,
        prefix: &str,
        log_dir: &Path,
        require_idempotency_key: bool,
        extra_env: &[(&str, &str)],
    ) -> Self {
        let stdout_path = log_dir.join(format!("{name}.stdout.log"));
        let stderr_path = log_dir.join(format!("{name}.stderr.log"));
        let stdout = File::create(&stdout_path).expect("create replica stdout log");
        let stderr = File::create(&stderr_path).expect("create replica stderr log");

        let process_database_url = url::Url::parse(database_url)
            .map(|mut parsed| {
                parsed
                    .query_pairs_mut()
                    .append_pair("application_name", instance_id);
                parsed.to_string()
            })
            .expect("acceptance PostgreSQL URL");

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
            .env("DATABASE_URL", &process_database_url)
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
            // The owner-death gate deliberately drives retries across the
            // reconciliation boundary. Keep this test focused on the durable
            // idempotency fence instead of the orthogonal per-process bucket.
            .env("IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE", "60000")
            .env("IRONCREW_ADMISSION_WORK_BURST", "1000")
            .env(
                "IRONCREW_REQUIRE_IDEMPOTENCY_KEY",
                require_idempotency_key.to_string(),
            )
            .env("IRONCREW_MAX_ACTIVE_RUNS", "2")
            .env("IRONCREW_MAX_SSE_CONNECTIONS", "4")
            .env("IRONCREW_MAX_RUN_LIFETIME", "60")
            .env("IRONCREW_SHUTDOWN_TIMEOUT_SECS", "3")
            .env("IRONCREW_SHUTDOWN_DRAIN_MS", "0")
            .env("RUST_LOG", "ironcrew=info")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));

        command.envs(extra_env.iter().copied());

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
            process_database_url,
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
        for (name, value) in extra_env {
            if *name != "IRONCREW_HITL_ENCRYPTION_KEYS" {
                continue;
            }
            diagnostic_redactions.push((*value).to_string());
            if let Ok(entries) =
                serde_json::from_str::<std::collections::HashMap<String, String>>(value)
            {
                diagnostic_redactions.extend(entries.into_values());
            }
        }
        diagnostic_redactions.sort();
        diagnostic_redactions.dedup();
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

    async fn wait_until_live(&mut self, client: &Client) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last_observation = String::from("no HTTP response");
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("inspect replica process") {
                panic!(
                    "{} exited before liveness with {status}\n{}",
                    self.name,
                    self.logs()
                );
            }
            match client
                .get(format!("{}/health/live", self.base_url))
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
            "{} did not become live ({last_observation})\n{}",
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

    fn sigkill(&mut self) -> ExitStatus {
        if let Some(status) = self
            .child
            .try_wait()
            .expect("inspect replica before SIGKILL")
        {
            panic!(
                "{} exited before the owner-death injection with {status}\n{}",
                self.name,
                self.logs()
            );
        }

        #[cfg(unix)]
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("send SIGKILL to owner replica");

        #[cfg(not(unix))]
        self.child.kill().expect("force-kill owner replica");

        self.child.wait().expect("reap SIGKILLed owner replica")
    }

    #[cfg(unix)]
    fn suspend_and_wait(&mut self) {
        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

        let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP)
            .expect("send SIGSTOP to replica");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED))
                .expect("observe stopped replica")
            {
                WaitStatus::Stopped(observed, nix::sys::signal::Signal::SIGSTOP)
                    if observed == pid =>
                {
                    return;
                }
                WaitStatus::StillAlive | WaitStatus::Continued(_) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => panic!(
                    "unexpected wait status after SIGSTOP for {}: {other:?}",
                    self.name
                ),
            }
        }
        panic!("{} did not enter SIGSTOP state", self.name);
    }

    #[cfg(unix)]
    fn resume(&mut self) {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.child.id() as i32),
            nix::sys::signal::Signal::SIGCONT,
        )
        .expect("send SIGCONT to replica");
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

#[cfg(unix)]
async fn wait_for_lease_renewal(
    client: &Client,
    base_url: &str,
    run_id: &str,
    previous_deadline: chrono::DateTime<chrono::Utc>,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_observation = String::from("no response");
    while Instant::now() < deadline {
        let response = authenticated(client.get(format!("{base_url}/flows/{FLOW}/runs/{run_id}")))
            .send()
            .await
            .expect("poll run lease renewal through peer");
        let status = response.status();
        let body = response.text().await.expect("read run lease response");
        last_observation = format!("HTTP {status}: {body}");
        if status == StatusCode::OK {
            let record: serde_json::Value =
                serde_json::from_str(&body).expect("parse run lease response");
            let renewed = record["lease_expires_at"]
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&chrono::Utc) > previous_deadline)
                .unwrap_or(false);
            if renewed {
                return record;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run lease did not renew after the stopped owner resumed; {last_observation}");
}

async fn flow_run_total(client: &Client, base_url: &str) -> u64 {
    authenticated(client.get(format!("{base_url}/flows/{FLOW}/runs?limit=100")))
        .send()
        .await
        .expect("list flow runs through surviving replica")
        .error_for_status()
        .expect("flow run list response")
        .json::<serde_json::Value>()
        .await
        .expect("parse flow run list")["total"]
        .as_u64()
        .expect("flow run total")
}

async fn durable_run_and_event_counts(database_url: &str, prefix: &str) -> (i64, i64) {
    let pool = sqlx::PgPool::connect(database_url)
        .await
        .expect("connect for durable replay counts");
    let statement = format!(
        "SELECT (SELECT COUNT(*) FROM {prefix}runs), \
                (SELECT COUNT(*) FROM {prefix}run_events)"
    );
    let counts = sqlx::query_as::<_, (i64, i64)>(sqlx::AssertSqlSafe(statement))
        .fetch_one(&pool)
        .await
        .expect("read durable replay counts");
    pool.close().await;
    counts
}

async fn replay_empty_keyed_run(client: &Client, base_url: &str, key: &str) -> Response {
    authenticated(client.post(format!("{base_url}/flows/{FLOW}/run")))
        .header("Idempotency-Key", key)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("replay keyed run through surviving replica")
}

async fn assert_owner_death_replay(response: Response, expected_run_id: &str) {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("Idempotency-Replayed")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    let replay: serde_json::Value = response.json().await.expect("parse owner-death replay");
    assert_eq!(replay["run_id"], expected_run_id);
    assert_eq!(replay["owner_instance_id"], "acceptance-replica-a");
    assert_eq!(replay["control_scope"], "process");
    assert_ne!(
        replay["owner_instance_id"], "acceptance-replica-b",
        "replay must not transfer execution ownership to the surviving replica"
    );
}

async fn replay_across_owner_reconciliation(
    client: &Client,
    base_url: &str,
    expected_run_id: &str,
) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut replay_count = 0;
    while Instant::now() < deadline {
        let response = replay_empty_keyed_run(client, base_url, OWNER_DEATH_IDEMPOTENCY_KEY).await;
        assert_owner_death_replay(response, expected_run_id).await;
        replay_count += 1;
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    replay_count
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
        // Bound connected stalls too. The longest intentional SSE read has a
        // 20-second application deadline, leaving ten seconds for transport
        // setup and teardown without permitting a hung CI job.
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build acceptance HTTP client");

    // Bootstrap the schema, then hold the exact global quota advisory lock
    // used by both reconciliation and startup idempotency pruning. The child
    // must still bind its liveness endpoint, advertise pessimistic readiness,
    // and recover through its ordinary maintenance loop after lock release.
    let bootstrap = PostgresStore::new_with_lease_config(
        &database_url,
        &prefix,
        RunLeaseConfig::new("startup-bootstrap", Duration::from_secs(6)).unwrap(),
    )
    .await
    .expect("bootstrap startup-lock acceptance schema");
    drop(bootstrap);
    let lock_pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect startup-lock holder");
    let mut quota_lock = lock_pool.begin().await.expect("begin startup quota lock");
    let quota_lock_name = format!("ironcrew:{prefix}idempotency:idempotency-quota:6:global");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&quota_lock_name)
        .execute(&mut *quota_lock)
        .await
        .expect("hold startup idempotency quota lock");

    let (startup_port, _) = reserve_two_ports();
    let mut startup_probe = ReplicaProcess::spawn(
        "startup-quota-lock",
        "acceptance-startup-lock",
        startup_port,
        temp.path(),
        &database_url,
        &prefix,
        temp.path(),
    );
    startup_probe.wait_until_live(&client).await;
    let blocked_ready = client
        .get(format!("{}/health/ready", startup_probe.base_url))
        .send()
        .await
        .expect("query readiness while startup quota lock is held");
    assert_eq!(blocked_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    let blocked_ready: serde_json::Value = blocked_ready
        .json()
        .await
        .expect("parse blocked startup readiness");
    assert_eq!(blocked_ready["component"], "storage_maintenance");

    quota_lock
        .rollback()
        .await
        .expect("release startup idempotency quota lock");
    lock_pool.close().await;
    startup_probe.wait_until_ready(&client).await;
    let startup_status = startup_probe.shutdown();
    if cfg!(unix) {
        assert!(
            startup_status.success(),
            "startup-lock replica shutdown failed: {}",
            startup_probe.logs()
        );
    }

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

    // Start a fresh two-process pair after proving graceful shutdown above.
    // This keeps the normal SIGTERM/reap coverage intact while the final run
    // deliberately destroys its owner with SIGKILL.
    let (owner_death_port_a, owner_death_port_b) = reserve_two_ports();
    let mut owner_death_a = ReplicaProcess::spawn(
        "owner-death-a",
        "acceptance-replica-a",
        owner_death_port_a,
        temp.path(),
        &database_url,
        &prefix,
        temp.path(),
    );
    let mut owner_death_b = ReplicaProcess::spawn(
        "owner-death-b",
        "acceptance-replica-b",
        owner_death_port_b,
        temp.path(),
        &database_url,
        &prefix,
        temp.path(),
    );
    tokio::join!(
        owner_death_a.wait_until_ready(&client),
        owner_death_b.wait_until_ready(&client)
    );
    assert_ne!(
        owner_death_a.id(),
        owner_death_b.id(),
        "owner-death replicas must be distinct PIDs"
    );

    let owner_death_started =
        authenticated(client.post(format!("{}/flows/{FLOW}/run", owner_death_a.base_url)))
            .header("Idempotency-Key", OWNER_DEATH_IDEMPOTENCY_KEY)
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("start owner-death run on replica A");
    assert_eq!(owner_death_started.status(), StatusCode::OK);
    assert!(
        owner_death_started
            .headers()
            .get("Idempotency-Replayed")
            .is_none()
    );
    let owner_death_acceptance: serde_json::Value = owner_death_started
        .json()
        .await
        .expect("parse owner-death run acceptance");
    let owner_death_run_id = owner_death_acceptance["run_id"]
        .as_str()
        .expect("owner-death run id")
        .to_string();
    assert_eq!(
        owner_death_acceptance["owner_instance_id"],
        "acceptance-replica-a"
    );

    // Waiting for the encrypted question proves the Lua coroutine is live,
    // durably owned by A, and parked long enough for the lease-expiry gate.
    wait_for_question(
        &client,
        &owner_death_b.base_url,
        &owner_death_run_id,
        FIRST_PROMPT,
    )
    .await;
    let active_before_kill: serde_json::Value = authenticated(client.get(format!(
        "{}/flows/{FLOW}/runs/{owner_death_run_id}",
        owner_death_b.base_url
    )))
    .send()
    .await
    .expect("observe owner-death run before SIGKILL")
    .error_for_status()
    .expect("active owner-death run")
    .json()
    .await
    .expect("parse active owner-death run");
    assert_eq!(active_before_kill["status"], "WaitingForInput");
    assert_eq!(
        active_before_kill["owner_instance_id"],
        "acceptance-replica-a"
    );

    // At the production minimum lease, deliberately miss two two-second
    // heartbeat opportunities without crossing the six-second fence. Resuming
    // A must renew the same durable run before B can classify it as dead.
    #[cfg(unix)]
    {
        let observed_deadline = chrono::DateTime::parse_from_rfc3339(
            active_before_kill["lease_expires_at"]
                .as_str()
                .expect("active run lease deadline"),
        )
        .expect("parse active run lease deadline")
        .with_timezone(&chrono::Utc);
        wait_for_lease_renewal(
            &client,
            &owner_death_b.base_url,
            &owner_death_run_id,
            observed_deadline,
        )
        .await;
        owner_death_a.suspend_and_wait();
        let stopped_record: serde_json::Value = authenticated(client.get(format!(
            "{}/flows/{FLOW}/runs/{owner_death_run_id}",
            owner_death_b.base_url
        )))
        .send()
        .await
        .expect("observe the confirmed-stopped owner's lease")
        .error_for_status()
        .expect("stopped owner's run remains observable")
        .json()
        .await
        .expect("parse stopped owner's run");
        let lease_while_stopped = chrono::DateTime::parse_from_rfc3339(
            stopped_record["lease_expires_at"]
                .as_str()
                .expect("stopped run lease deadline"),
        )
        .expect("parse stopped run lease deadline")
        .with_timezone(&chrono::Utc);
        assert!(
            lease_while_stopped > chrono::Utc::now(),
            "the confirmed-stopped owner must still have time to recover"
        );
        tokio::time::sleep(Duration::from_millis(4_250)).await;

        let while_suspended: serde_json::Value = authenticated(client.get(format!(
            "{}/flows/{FLOW}/runs/{owner_death_run_id}",
            owner_death_b.base_url
        )))
        .send()
        .await
        .expect("observe run after two missed heartbeats")
        .error_for_status()
        .expect("healthy run after two missed heartbeats")
        .json()
        .await
        .expect("parse run after two missed heartbeats");
        assert_eq!(while_suspended["status"], "WaitingForInput");
        assert_eq!(while_suspended["owner_instance_id"], "acceptance-replica-a");
        assert_eq!(
            while_suspended["lease_expires_at"], stopped_record["lease_expires_at"],
            "a stopped keyed owner cannot renew through the peer's broad heartbeat"
        );

        owner_death_a.resume();
        let renewed = wait_for_lease_renewal(
            &client,
            &owner_death_b.base_url,
            &owner_death_run_id,
            lease_while_stopped,
        )
        .await;
        assert_eq!(renewed["status"], "WaitingForInput");
        assert_eq!(renewed["owner_instance_id"], "acceptance-replica-a");
        owner_death_a.wait_until_ready(&client).await;
    }

    // Baseline while A is demonstrably live so later reconciliation and retry
    // traffic cannot hide a second durable launch behind the snapshot work.
    let runs_before_replay = flow_run_total(&client, &owner_death_b.base_url).await;
    let durable_counts_before_replay = durable_run_and_event_counts(&database_url, &prefix).await;

    let killed_owner_pid = owner_death_a.id();
    let killed_owner_status = owner_death_a.sigkill();
    assert!(
        !killed_owner_status.success(),
        "SIGKILLed owner PID {killed_owner_pid} exited successfully"
    );
    #[cfg(unix)]
    assert_eq!(
        killed_owner_status.signal(),
        Some(nix::sys::signal::Signal::SIGKILL as i32),
        "owner PID {killed_owner_pid} did not exit from SIGKILL"
    );

    // Prove the retained response is already replay-safe before the dead
    // owner's lease expires. The survivor must not wait for Abandoned before
    // preventing a duplicate launch.
    let pre_reconciliation_replay = replay_empty_keyed_run(
        &client,
        &owner_death_b.base_url,
        OWNER_DEATH_IDEMPOTENCY_KEY,
    )
    .await;
    assert_owner_death_replay(pre_reconciliation_replay, &owner_death_run_id).await;
    let pre_reconciliation_run: serde_json::Value = authenticated(client.get(format!(
        "{}/flows/{FLOW}/runs/{owner_death_run_id}",
        owner_death_b.base_url
    )))
    .send()
    .await
    .expect("observe dead owner's run before lease reconciliation")
    .error_for_status()
    .expect("pre-reconciliation owner-death run")
    .json()
    .await
    .expect("parse pre-reconciliation owner-death run");
    assert_eq!(pre_reconciliation_run["status"], "WaitingForInput");
    assert_eq!(
        pre_reconciliation_run["owner_instance_id"],
        "acceptance-replica-a"
    );

    // Replica B's normal heartbeat/reconciler loop uses the database clock.
    // After A's six-second lease expires, B must terminalize the exact run as
    // Abandoned, clear the mailbox, and complete the idempotency tombstone.
    let (abandoned, reconciliation_replays) = tokio::join!(
        wait_for_terminal(
            &client,
            &owner_death_b.base_url,
            &owner_death_run_id,
            "Abandoned",
        ),
        replay_across_owner_reconciliation(&client, &owner_death_b.base_url, &owner_death_run_id,)
    );
    assert!(
        reconciliation_replays >= 4,
        "expected sustained replay coverage across reconciliation, got {reconciliation_replays}"
    );
    assert_eq!(abandoned["run_id"], owner_death_run_id);
    assert_eq!(abandoned["owner_instance_id"], "acceptance-replica-a");
    assert_eq!(abandoned["lease_expires_at"], "");

    let questions_after_reconcile = authenticated(client.get(format!(
        "{}/flows/{FLOW}/questions/{owner_death_run_id}",
        owner_death_b.base_url
    )))
    .send()
    .await
    .expect("query reconciled human-input mailbox");
    assert_eq!(questions_after_reconcile.status(), StatusCode::NOT_FOUND);
    let questions_after_reconcile = questions_after_reconcile
        .text()
        .await
        .expect("read reconciled mailbox response");
    assert!(!questions_after_reconcile.contains(FIRST_PROMPT));

    let (replay_one, replay_two, replay_three, replay_four) = tokio::join!(
        replay_empty_keyed_run(
            &client,
            &owner_death_b.base_url,
            OWNER_DEATH_IDEMPOTENCY_KEY
        ),
        replay_empty_keyed_run(
            &client,
            &owner_death_b.base_url,
            OWNER_DEATH_IDEMPOTENCY_KEY
        ),
        replay_empty_keyed_run(
            &client,
            &owner_death_b.base_url,
            OWNER_DEATH_IDEMPOTENCY_KEY
        ),
        replay_empty_keyed_run(
            &client,
            &owner_death_b.base_url,
            OWNER_DEATH_IDEMPOTENCY_KEY
        )
    );
    for owner_death_replay in [replay_one, replay_two, replay_three, replay_four] {
        assert_owner_death_replay(owner_death_replay, &owner_death_run_id).await;
    }

    // The ten-second transition-spanning replay loop plus this post-tombstone
    // burst must not publish another durable run or Lua event.
    assert_eq!(
        flow_run_total(&client, &owner_death_b.base_url).await,
        runs_before_replay,
        "same-key replay created a second run"
    );
    assert_eq!(
        durable_run_and_event_counts(&database_url, &prefix).await,
        durable_counts_before_replay,
        "same-key replay created a run row or published a durable run event"
    );
    let still_abandoned = wait_for_terminal(
        &client,
        &owner_death_b.base_url,
        &owner_death_run_id,
        "Abandoned",
    )
    .await;
    assert_eq!(still_abandoned["owner_instance_id"], "acceptance-replica-a");

    let questions_after_replay = authenticated(client.get(format!(
        "{}/flows/{FLOW}/questions/{owner_death_run_id}",
        owner_death_b.base_url
    )))
    .send()
    .await
    .expect("query human-input mailbox after replay");
    assert_eq!(questions_after_replay.status(), StatusCode::NOT_FOUND);
    let questions_after_replay = questions_after_replay
        .text()
        .await
        .expect("read mailbox response after replay");
    assert!(!questions_after_replay.contains(FIRST_PROMPT));
    assert!(!questions_after_replay.contains(SECOND_PROMPT));

    let survivor_ready = client
        .get(format!("{}/health/ready", owner_death_b.base_url))
        .send()
        .await
        .expect("query surviving replica readiness");
    assert_eq!(survivor_ready.status(), StatusCode::OK);
    let owner_death_b_status = owner_death_b.shutdown();
    if cfg!(unix) {
        assert!(
            owner_death_b_status.success(),
            "surviving replica shutdown failed: {}",
            owner_death_b.logs()
        );
    }

    reset_schema(&database_url, &prefix).await;
}

use reqwest::StatusCode;

use super::*;

pub(super) const FIXTURE: &str = include_str!("../../fixtures/two_process_replica/ic019_crew.lua");
pub(super) const PROMPT: &str = "Hold IC-019 admission capacity";

pub(super) const REPLICAS: usize = 2;

pub(super) const ROOMY_ENV: &[(&str, &str)] = &[
    ("IRONCREW_MAX_ACTIVE_RUNS", "1"),
    ("IRONCREW_MAX_ACTIVE_CONVERSATIONS", "1"),
    ("IRONCREW_MAX_SSE_CONNECTIONS", "1"),
    ("IRONCREW_DB_POOL_SIZE", "2"),
    ("IRONCREW_LUA_MAX_MEMORY_BYTES", "16777216"),
    ("IRONCREW_DEFAULT_MAX_CONCURRENT", "1"),
    ("IRONCREW_MAX_CONCURRENT_TASKS", "1"),
    ("IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_WORK_BURST", "100"),
    ("IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_CONTROL_BURST", "100"),
    ("IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_OBSERVATION_BURST", "100"),
];

pub(super) const RATE_ENV: &[(&str, &str)] = &[
    ("IRONCREW_MAX_ACTIVE_RUNS", "2"),
    ("IRONCREW_MAX_ACTIVE_CONVERSATIONS", "2"),
    ("IRONCREW_MAX_SSE_CONNECTIONS", "2"),
    ("IRONCREW_DB_POOL_SIZE", "2"),
    ("IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE", "1"),
    ("IRONCREW_ADMISSION_WORK_BURST", "1"),
    ("IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE", "1"),
    ("IRONCREW_ADMISSION_CONTROL_BURST", "1"),
    ("IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE", "1"),
    ("IRONCREW_ADMISSION_OBSERVATION_BURST", "1"),
];

pub(super) const QUOTA_ENV: &[(&str, &str)] = &[
    ("IRONCREW_MAX_ACTIVE_RUNS", "2"),
    ("IRONCREW_DB_POOL_SIZE", "2"),
    ("IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_WORK_BURST", "100"),
    ("IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_CONTROL_BURST", "100"),
    ("IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_OBSERVATION_BURST", "100"),
    ("IRONCREW_IDEMPOTENCY_TTL_SECONDS", "3660"),
    ("IRONCREW_IDEMPOTENCY_MAX_RECORDS", "1"),
    ("IRONCREW_IDEMPOTENCY_MAX_RECORDS_PER_PRINCIPAL", "1"),
    ("IRONCREW_IDEMPOTENCY_MAX_IN_FLIGHT_PER_PRINCIPAL", "1"),
];

pub(super) fn assert_capacity_envelope() {
    let configured = |name: &str| {
        ROOMY_ENV
            .iter()
            .find_map(|(key, value)| (*key == name).then_some(*value))
            .unwrap_or_else(|| panic!("missing IC-019 capacity setting {name}"))
            .parse::<usize>()
            .unwrap_or_else(|error| panic!("invalid IC-019 capacity setting {name}: {error}"))
    };
    let active_runs = configured("IRONCREW_MAX_ACTIVE_RUNS");
    let active_conversations = configured("IRONCREW_MAX_ACTIVE_CONVERSATIONS");
    let sse = configured("IRONCREW_MAX_SSE_CONNECTIONS");
    let pg_pool = configured("IRONCREW_DB_POOL_SIZE");
    let lua_bytes = configured("IRONCREW_LUA_MAX_MEMORY_BYTES");
    let run_phase_slots = configured("IRONCREW_DEFAULT_MAX_CONCURRENT");

    assert_eq!(REPLICAS * active_runs, 2);
    assert_eq!(REPLICAS * active_conversations, 2);
    assert_eq!(REPLICAS * sse, 2);
    assert_eq!(REPLICAS * pg_pool, 4);
    assert_eq!(
        REPLICAS * (active_runs + active_conversations) * lua_bytes,
        64 * 1024 * 1024
    );
    assert_eq!(REPLICAS * active_runs * run_phase_slots, 2);
    // These are configured planning values. The Lua result covers admitted
    // top-level VMs only: nested sub-flow VMs add their own caps, and the value
    // is not RSS. Per-run task slots are not a process-global provider
    // semaphore.
}

pub(super) async fn wait_for_waiting(pair: &ProcessPair, run_id: &str) {
    let record = wait_for_status(pair, run_id, "WaitingForInput").await;
    assert_eq!(record["run_id"], run_id);

    // The durable status transition and encrypted mailbox insert are separate
    // writes. Wait for both before spending a burst=1 observation request so
    // the gate cannot mistake that short registration window for rate-limit
    // isolation.
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-019 mailbox readiness");
    let sql = format!(
        "SELECT COUNT(*) FROM {}human_inputs WHERE run_id = $1 AND state = 'pending'",
        pair.prefix
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let pending = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.clone()))
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("read IC-019 pending mailbox count");
        if pending == 1 {
            pool.close().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    pool.close().await;
    panic!("IC-019 pending mailbox did not become visible for {run_id}");
}

pub(super) async fn assert_aborted(pair: &ProcessPair, run_id: &str) {
    let record = wait_for_status(pair, run_id, "Aborted").await;
    assert_eq!(record["run_id"], run_id);
}

pub(super) async fn assert_question(response: reqwest::Response) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    let body: serde_json::Value = response.json().await.expect("parse IC-019 question body");
    assert_eq!(body["status"], "waiting_for_input");
    assert!(body["questions"].as_array().is_some_and(|questions| {
        questions
            .iter()
            .any(|question| question["prompt"] == PROMPT)
    }));
    body
}

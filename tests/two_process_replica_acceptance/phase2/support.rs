use ironcrew::engine::postgres_store::PostgresStore;
use ironcrew::engine::run_history::{RunCompletion, RunStatus, RunTransition};
use ironcrew::engine::store::{RunLeaseConfig, StateStore};
use sqlx::{Connection, PgConnection, Row};

use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ScopedSnapshot {
    pub(super) runs: i64,
    pub(super) status: Option<String>,
    pub(super) run_owner: Option<String>,
    pub(super) run_lease: Option<String>,
    pub(super) run_finished: Option<String>,
    pub(super) ledgers: i64,
    pub(super) ledger_state: Option<String>,
    pub(super) ledger_owner: Option<String>,
    pub(super) ledger_lease: Option<String>,
    pub(super) cancel_requested: bool,
    pub(super) response_status: Option<i32>,
    pub(super) response_body: Option<String>,
    pub(super) ledger_completed: Option<String>,
    pub(super) ledger_expires: Option<String>,
    pub(super) mailbox: i64,
    pub(super) events: i64,
    pub(super) human_requested: i64,
    pub(super) run_complete: i64,
    pub(super) journal_complete: Option<bool>,
    pub(super) terminal_sequence: Option<i64>,
    pub(super) abort_audits: i64,
    pub(super) valid_abort_audits: i64,
}

pub(super) async fn snapshot(pair: &ProcessPair, run_id: &str) -> ScopedSnapshot {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for phase-two snapshot");
    let sql = format!(
        "SELECT (SELECT COUNT(*) FROM {p}runs WHERE run_id=$1) runs, \
         (SELECT status FROM {p}runs WHERE run_id=$1) status, \
         (SELECT owner_instance_id FROM {p}runs WHERE run_id=$1) run_owner, \
         (SELECT lease_expires_at FROM {p}runs WHERE run_id=$1) run_lease, \
         (SELECT finished_at FROM {p}runs WHERE run_id=$1) run_finished, \
         (SELECT COUNT(*) FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1) ledgers, \
         (SELECT state FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1) ledger_state, \
         (SELECT owner_instance_id FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1) ledger_owner, \
         (SELECT lease_expires_at FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1) ledger_lease, \
         COALESCE((SELECT cancel_requested_at IS NOT NULL FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1),FALSE) cancel_requested, \
         (SELECT response_status FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1) response_status, \
         (SELECT response_body FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1) response_body, \
         (SELECT completed_at FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1) ledger_completed, \
         (SELECT expires_at FROM {p}idempotency WHERE operation='flow.run' AND resource_id=$1 LIMIT 1) ledger_expires, \
         (SELECT COUNT(*) FROM {p}human_inputs WHERE run_id=$1) mailbox, \
         (SELECT COUNT(*) FROM {p}run_events WHERE run_id=$1) events, \
         (SELECT COUNT(*) FROM {p}run_events WHERE run_id=$1 AND event_type='human_input_requested') human_requested, \
         (SELECT COUNT(*) FROM {p}run_events WHERE run_id=$1 AND event_type='run_complete') run_complete, \
         (SELECT journal_complete FROM {p}run_event_state WHERE run_id=$1) journal_complete, \
         (SELECT terminal_event_sequence FROM {p}run_event_state WHERE run_id=$1) terminal_sequence, \
         (SELECT COUNT(*) FROM {p}audit_events WHERE action='flow.run.abort' AND target=$1) abort_audits, \
         (SELECT COUNT(*) FROM {p}audit_events WHERE action='flow.run.abort' AND target=$1 \
           AND actor='acceptance-client' AND success=TRUE AND status_code=200) valid_abort_audits",
        p = pair.prefix
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read phase-two snapshot");
    let result = ScopedSnapshot {
        runs: row.get("runs"),
        status: row.get("status"),
        run_owner: row.get("run_owner"),
        run_lease: row.get("run_lease"),
        run_finished: row.get("run_finished"),
        ledgers: row.get("ledgers"),
        ledger_state: row.get("ledger_state"),
        ledger_owner: row.get("ledger_owner"),
        ledger_lease: row.get("ledger_lease"),
        cancel_requested: row.get("cancel_requested"),
        response_status: row.get("response_status"),
        response_body: row.get("response_body"),
        ledger_completed: row.get("ledger_completed"),
        ledger_expires: row.get("ledger_expires"),
        mailbox: row.get("mailbox"),
        events: row.get("events"),
        human_requested: row.get("human_requested"),
        run_complete: row.get("run_complete"),
        journal_complete: row.get("journal_complete"),
        terminal_sequence: row.get("terminal_sequence"),
        abort_audits: row.get("abort_audits"),
        valid_abort_audits: row.get("valid_abort_audits"),
    };
    pool.close().await;
    result
}

pub(super) struct AdvisoryLock {
    connection: PgConnection,
    pub(super) backend_pid: i32,
    lock_name: String,
}

impl AdvisoryLock {
    pub(super) async fn quota(pair: &ProcessPair) -> Self {
        let lock_name = format!(
            "ironcrew:{}idempotency:idempotency-quota:6:global",
            pair.prefix
        );
        let mut connection = PgConnection::connect(&pair.database_url)
            .await
            .expect("connect direct advisory-lock session");
        let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut connection)
            .await
            .expect("read advisory-lock backend pid");
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(&lock_name)
            .execute(&mut connection)
            .await
            .expect("hold idempotency quota advisory lock");
        Self {
            connection,
            backend_pid,
            lock_name,
        }
    }

    pub(super) async fn release(mut self) {
        sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(&self.lock_name)
            .execute(&mut self.connection)
            .await
            .expect("release idempotency quota advisory lock");
        self.connection
            .close()
            .await
            .expect("close advisory-lock session");
    }
}

pub(super) async fn wait_for_log(process: &ReplicaProcess, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if process.logs().contains(marker) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("process log did not contain {marker:?}\n{}", process.logs());
}

pub(super) async fn wait_until_blocked(pair: &ProcessPair, blocker_pid: i32) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for blocker probe");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 \
             AND $2 = ANY(pg_blocking_pids(pid)))",
        )
        .bind(&pair.owner_a_id)
        .bind(blocker_pid)
        .fetch_one(&pool)
        .await
        .expect("probe blocked owner");
        if blocked {
            pool.close().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    pool.close().await;
    panic!("owner process never blocked behind advisory-lock backend {blocker_pid}");
}

pub(super) async fn assert_stale_completion_fenced(pair: &ProcessPair, run_id: &str) {
    let store = PostgresStore::new_with_lease_config(
        &pair.database_url,
        &pair.prefix,
        RunLeaseConfig::new(pair.owner_a_id.clone(), Duration::from_secs(6)).unwrap(),
    )
    .await
    .expect("open stale-owner store");
    let transition = store
        .update_run_completion(
            run_id,
            RunCompletion {
                status: RunStatus::Success,
                finished_at: chrono::Utc::now().to_rfc3339(),
                duration_ms: 1,
                task_results: vec![],
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .expect("late owner completion result");
    assert_eq!(
        transition,
        RunTransition::AlreadyTerminal(RunStatus::Abandoned)
    );
    assert_eq!(
        store.get_run(run_id).await.expect("read fenced run").status,
        RunStatus::Abandoned
    );
}

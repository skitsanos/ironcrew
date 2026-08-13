use super::super::*;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DurableSnapshot {
    pub(super) runs: i64,
    pub(super) events: i64,
    pub(super) idempotency: i64,
    pub(super) pending_questions: i64,
    pub(super) answered_questions: i64,
    pub(super) cancellation_requests: i64,
}

pub(super) async fn snapshot(pair: &ProcessPair) -> DurableSnapshot {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect IC-020 SQL observer");
    let statement = format!(
        "SELECT \
             (SELECT COUNT(*) FROM {p}runs), \
             (SELECT COUNT(*) FROM {p}run_events), \
             (SELECT COUNT(*) FROM {p}idempotency), \
             (SELECT COUNT(*) FROM {p}human_inputs WHERE state = 'pending'), \
             (SELECT COUNT(*) FROM {p}human_inputs WHERE state = 'answered'), \
             (SELECT COUNT(*) FROM {p}idempotency \
                 WHERE cancel_requested_at IS NOT NULL)",
        p = pair.prefix,
    );
    let values: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(statement))
        .fetch_one(&pool)
        .await
        .expect("read IC-020 durable snapshot");
    pool.close().await;
    DurableSnapshot {
        runs: values.0,
        events: values.1,
        idempotency: values.2,
        pending_questions: values.3,
        answered_questions: values.4,
        cancellation_requests: values.5,
    }
}

pub(super) async fn wait_for_snapshot(
    pair: &ProcessPair,
    label: &str,
    predicate: impl Fn(DurableSnapshot) -> bool,
) -> DurableSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut observed = snapshot(pair).await;
    while Instant::now() < deadline {
        if predicate(observed) {
            return observed;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        observed = snapshot(pair).await;
    }
    panic!("IC-020 durable snapshot did not reach {label}: {observed:?}");
}

pub(super) async fn assert_owner_draining(pair: &ProcessPair, run_id: &str) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect IC-020 owner-drain observer");
    let statement = format!(
        "SELECT COUNT(*) = 1 FROM {}idempotency \
         WHERE operation = $1 AND resource_id = $2 \
           AND owner_instance_id = $3 AND attempt_id <> '' \
           AND state = 'running' \
           AND owner_draining_at IS NOT NULL AND owner_draining_at <> ''",
        pair.prefix
    );
    let draining: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
        .bind(ironcrew::engine::idempotency::RUN_OPERATION)
        .bind(run_id)
        .bind(&pair.owner_a_id)
        .fetch_one(&pool)
        .await
        .expect("read IC-020 owner-drain fence");
    pool.close().await;
    assert!(draining, "IC-020 run owner was not durably fenced");
}

pub(super) async fn wait_owner_draining(pair: &ProcessPair, run_id: &str) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect IC-020 owner-drain waiter");
    let statement = format!(
        "SELECT state FROM {}idempotency \
         WHERE operation = $1 AND resource_id = $2 \
           AND owner_instance_id = $3 AND attempt_id <> '' \
           AND owner_draining_at IS NOT NULL AND owner_draining_at <> ''",
        pair.prefix
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let state: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(statement.clone()))
            .bind(ironcrew::engine::idempotency::RUN_OPERATION)
            .bind(run_id)
            .bind(&pair.owner_a_id)
            .fetch_optional(&pool)
            .await
            .expect("read IC-020 committed owner-drain fence");
        if let Some(state) = state {
            pool.close().await;
            assert!(
                matches!(state.as_str(), "running" | "completed"),
                "IC-020 owner-drain fence committed in unexpected ledger state {state:?}"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    pool.close().await;
    panic!("IC-020 owner-drain fence did not commit for run {run_id}");
}

pub(super) async fn assert_owner_not_draining(pair: &ProcessPair, run_id: &str) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect IC-020 pre-fence observer");
    let statement = format!(
        "SELECT COUNT(*) = 1 FROM {}idempotency \
         WHERE operation=$1 AND resource_id=$2 AND owner_instance_id=$3 \
           AND state='running' AND owner_draining_at IS NULL",
        pair.prefix,
    );
    let absent: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
        .bind(ironcrew::engine::idempotency::RUN_OPERATION)
        .bind(run_id)
        .bind(&pair.owner_a_id)
        .fetch_one(&pool)
        .await
        .expect("read IC-020 absent owner fence");
    pool.close().await;
    assert!(absent, "owner drain marker appeared before fence commit");
}

pub(super) async fn assert_terminal_fences(
    pair: &ProcessPair,
    run_id: &str,
    owner_instance_id: &str,
) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect IC-020 terminal observer");
    let statement = format!(
        "SELECT run.status, run.owner_instance_id run_owner, run.lease_expires_at run_lease, \
                run.finished_at, idem.state ledger_state, \
                idem.owner_instance_id ledger_owner, idem.lease_expires_at ledger_lease, \
                idem.cancel_requested_at IS NOT NULL cancelled, \
                idem.owner_draining_at IS NOT NULL owner_draining, \
                idem.completed_at, idem.expires_at, idem.response_status, \
                idem.response_body::jsonb ->> 'run_id' replay_run_id \
         FROM {p}runs run JOIN {p}idempotency idem \
           ON idem.operation=$1 AND idem.resource_id=run.run_id \
         WHERE run.run_id=$2",
        p = pair.prefix,
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(ironcrew::engine::idempotency::RUN_OPERATION)
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read IC-020 terminal fences");
    pool.close().await;
    assert_eq!(row.get::<String, _>("status"), "aborted");
    assert_eq!(row.get::<String, _>("run_owner"), owner_instance_id);
    assert_eq!(row.get::<String, _>("run_lease"), "");
    assert!(!row.get::<String, _>("finished_at").is_empty());
    assert_eq!(row.get::<String, _>("ledger_state"), "completed");
    assert_eq!(row.get::<String, _>("ledger_owner"), owner_instance_id);
    assert_eq!(row.get::<String, _>("ledger_lease"), "");
    assert!(!row.get::<bool, _>("cancelled"));
    assert!(row.get::<bool, _>("owner_draining"));
    assert!(
        row.get::<Option<String>, _>("completed_at")
            .is_some_and(|value| !value.is_empty())
    );
    assert!(
        row.get::<Option<String>, _>("expires_at")
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(row.get::<Option<i32>, _>("response_status"), Some(200));
    assert_eq!(
        row.get::<Option<String>, _>("replay_run_id").as_deref(),
        Some(run_id)
    );
}

pub(super) async fn assert_owner_index(pair: &ProcessPair) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect IC-020 index observer");
    let table = format!("{}idempotency", pair.prefix);
    let index = format!("{table}_owner_idx");
    let row: (bool, bool, String, Option<String>) = sqlx::query_as(
        "SELECT ind.indisvalid, ind.indisready, pg_get_indexdef(ind.indexrelid), \
                pg_get_expr(ind.indpred, ind.indrelid) \
         FROM pg_index ind \
         JOIN pg_class idx ON idx.oid = ind.indexrelid \
         JOIN pg_class tbl ON tbl.oid = ind.indrelid \
         JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
         WHERE ns.nspname = current_schema() AND tbl.relname = $1 AND idx.relname = $2",
    )
    .bind(table)
    .bind(index)
    .fetch_one(&pool)
    .await
    .expect("read IC-020 owner-drain index");
    pool.close().await;
    assert!(row.0 && row.1, "owner-drain index is not usable");
    assert!(
        row.2.contains("(owner_instance_id, operation)"),
        "{}",
        row.2
    );
    let predicate = row.3.expect("IC-020 owner index partial predicate");
    for expected in ["state", "claimed", "running"] {
        assert!(predicate.contains(expected), "{predicate}");
    }
}

pub(super) fn assert_initial(snapshot: DurableSnapshot) {
    assert_eq!(snapshot.runs, 1);
    assert_eq!(snapshot.idempotency, 1);
    assert_eq!(snapshot.pending_questions, 1);
    assert_eq!(snapshot.answered_questions, 0);
    assert_eq!(snapshot.cancellation_requests, 0);
    assert_eq!(snapshot.events, 1, "unexpected held-run journal events");
}

pub(super) fn assert_one_terminal_event(before: DurableSnapshot, after: DurableSnapshot) {
    assert_eq!(after.runs, before.runs);
    assert_eq!(after.events, before.events + 1);
    assert_eq!(after.idempotency, before.idempotency);
    assert_eq!(after.pending_questions, 0);
    assert_eq!(after.answered_questions, 0);
    assert_eq!(after.cancellation_requests, 0);
}

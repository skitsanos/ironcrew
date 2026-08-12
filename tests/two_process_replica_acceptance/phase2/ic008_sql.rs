use sqlx::Row;

use super::*;

const OPERATION: &str = "conversation.message";

pub(super) async fn wait_for_active_turn(pair: &ProcessPair, id: &str) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-008 active-turn evidence");
    let sql = format!(
        "SELECT COUNT(*) FROM {prefix}idempotency WHERE operation=$1 AND scope=$2 \
         AND resource_id=$3 AND state IN ('claimed', 'running')",
        prefix = pair.prefix,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let active: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.clone()))
            .bind(OPERATION)
            .bind(FLOW)
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read IC-008 active turn");
        if active == 1 {
            pool.close().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    pool.close().await;
    panic!("IC-008 keyed turn did not retain one active durable claim");
}

pub(super) async fn assert_final_state(pair: &ProcessPair, expected_terminal: i64) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-008 terminal evidence");
    let conversations_table = format!("{}conversations", pair.prefix);
    let conversations: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM {conversations_table}"
    )))
    .fetch_one(&pool)
    .await
    .expect("count IC-008 conversations");
    assert_eq!(conversations, 0, "IC-008 final conversation cleanup");

    let idempotency_table = format!("{}idempotency", pair.prefix);
    let sql = format!(
        "SELECT \
           COUNT(*) FILTER (WHERE operation=$1) AS total, \
           COUNT(*) FILTER (WHERE operation=$1 AND state='completed') AS completed, \
           COUNT(*) FILTER (WHERE operation=$1 AND state IN ('claimed', 'running')) AS active, \
           COUNT(*) FILTER (WHERE operation=$1 AND (response_status <> 200 \
             OR response_body IS NULL OR lease_expires_at <> '' \
             OR completed_at IS NULL OR expires_at IS NULL)) AS malformed_terminal \
         FROM {idempotency_table}"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(OPERATION)
        .fetch_one(&pool)
        .await
        .expect("read IC-008 terminal ledger");
    let observed = (
        row.get::<i64, _>("total"),
        row.get::<i64, _>("completed"),
        row.get::<i64, _>("active"),
        row.get::<i64, _>("malformed_terminal"),
    );
    assert_eq!(
        observed,
        (expected_terminal, expected_terminal, 0, 0),
        "IC-008 terminal ledger cleanup"
    );
    pool.close().await;
}

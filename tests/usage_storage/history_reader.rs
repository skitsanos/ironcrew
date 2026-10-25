#![cfg(feature = "postgres")]

use ironcrew::history_reader::PostgresHistoryReader;
use ironcrew::usage::UsageSnapshot;
use serde_json::json;

#[tokio::test]
async fn read_only_history_preserves_rows_and_rejects_invalid_checked_usage() {
    let Ok(url) = std::env::var("IRONCREW_TEST_PG_URL") else {
        eprintln!("SKIP live history reader: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("hr_{}", &suffix[..16]);
    let table = format!("{prefix}runs");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {table} (run_id TEXT PRIMARY KEY,flow_name TEXT NOT NULL,flow TEXT NOT NULL,
         status TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT NOT NULL,duration_ms BIGINT,
         agent_count INT,task_count INT,task_results JSONB,usage JSONB)"
    )))
    .execute(&pool)
    .await
    .unwrap();
    for (id, flow, usage) in [
        ("a", "", json!(UsageSnapshot::unavailable())),
        ("b", "retired-flow", json!(UsageSnapshot::unavailable())),
        ("bad", "retired-flow", json!({"coverage":"complete"})),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {table} VALUES($1,'old goal',$2,'Success',
            '2026-01-01T00:00:00Z','2026-01-01T00:00:01Z',1,1,1,$3,$4)"
        )))
        .bind(id)
        .bind(flow)
        .bind(
            json!([{"task":"summary","agent":"writer","output":"authored fixture",
                "success":true,"duration_ms":1,"token_usage":{"total_tokens":99}}]),
        )
        .bind(usage)
        .execute(&pool)
        .await
        .unwrap();
    }
    let snapshot = format!("SELECT row_to_json(r)::text FROM {table} r ORDER BY run_id");
    let before: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(snapshot.clone()))
        .fetch_all(&pool)
        .await
        .unwrap();
    // Hosted CI intentionally uses NOCREATEROLE. A separate restore rehearsal proves SELECT-only
    // credentials; this fixture uses the existing restricted CI account without escalating it.
    let mut reader = PostgresHistoryReader::connect(&url, &prefix).await.unwrap();
    let page = reader.list(None, 1, 0).await.unwrap();
    assert!(page.read_only && page.has_more);
    assert_eq!(page.runs.len(), 1);
    assert_eq!(reader.list(Some(""), 100, 0).await.unwrap().runs.len(), 1);
    assert_eq!(
        reader
            .list(Some("retired-flow"), 100, 0)
            .await
            .unwrap()
            .runs
            .len(),
        2
    );
    let legacy = reader.inspect("a", Some("")).await.unwrap();
    assert!(legacy.tasks[0].usage.is_none());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(legacy.record.get()).unwrap()["task_results"][0]
            ["token_usage"]["total_tokens"],
        99
    );
    assert_eq!(legacy.run_usage, Some(UsageSnapshot::unavailable()));
    assert!(reader.inspect("a", Some("retired-flow")).await.is_err());
    assert!(reader.inspect("missing", None).await.is_err());
    assert!(reader.inspect("bad", None).await.is_err());
    assert!(reader.list(None, 101, 0).await.is_err());
    assert!(reader.list(None, 0, 0).await.is_err());
    assert!(reader.list(None, 1, 10_001).await.is_err());
    assert!(reader.inspect("", None).await.is_err());
    assert!(reader.inspect(&"x".repeat(256), None).await.is_err());
    assert!(reader.inspect("a", Some("bad\nflow")).await.is_err());
    for prefix in ["Invalid_", "bad-", "x;DROP TABLE runs;--", &"x".repeat(38)] {
        assert!(PostgresHistoryReader::connect(&url, prefix).await.is_err());
    }
    let after: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(snapshot))
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    // Large server-side values must be rejected by SQL before transporting record bytes.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {table}
        SELECT 'big','old goal','','success','z','z',1,1,1,
        jsonb_build_array(jsonb_build_object('task','t','agent','a','success',true,
        'duration_ms',1,'output',repeat('x',8388608))),$1"
    )))
    .bind(json!(UsageSnapshot::unavailable()))
    .execute(&pool)
    .await
    .unwrap();
    let error = reader.inspect("big", None).await.unwrap_err();
    assert!(error.to_string().contains("8 MiB"));
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {table}
        SELECT 'summary_big',repeat('x',16384),'','success','zz','zz',1,1,0,'[]'::jsonb,$1"
    )))
    .bind(json!(UsageSnapshot::unavailable()))
    .execute(&pool)
    .await
    .unwrap();
    let error = reader.list(None, 1, 0).await.unwrap_err();
    assert!(error.to_string().contains("16 KiB"));
    drop(reader);
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {table}")))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

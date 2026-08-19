#![cfg(feature = "postgres")]
//! Live app-db tests. Skipped unless IRONCREW_TEST_PG_URL points at a
//! disposable database (never shared/production infrastructure — AGENTS.md).

use ironcrew::engine::app_db::AppDb;
use ironcrew::engine::app_db::operations::OperationRegistry;
use ironcrew::engine::app_db::policy::AppDbPolicy;

fn test_url() -> Option<String> {
    std::env::var("IRONCREW_TEST_PG_URL")
        .ok()
        .filter(|v| !v.is_empty())
}

fn policy() -> AppDbPolicy {
    AppDbPolicy::from_values(4, 2_000, 500, 1 << 20, 1 << 20, 64, 64 * 1024)
}

fn app(url: &str, ops: Vec<(&str, &str)>) -> AppDb {
    let sources = ops
        .into_iter()
        .map(|(n, s)| (n.to_string(), s.to_string()))
        .collect();
    let registry = OperationRegistry::from_sources(sources, &policy()).unwrap();
    AppDb::new(url.to_string(), policy(), registry)
}

const SETUP: &str = "-- ironcrew:op\nCREATE TABLE IF NOT EXISTS app_db_test (\n  key text PRIMARY KEY, payload jsonb NOT NULL, n bigint NOT NULL DEFAULT 0);";
const TEARDOWN: &str = "-- ironcrew:op\nDROP TABLE IF EXISTS app_db_test;";
const UPSERT: &str = "-- ironcrew:op\n-- params: key text, payload json\nINSERT INTO app_db_test (key, payload) VALUES ($1, $2)\nON CONFLICT (key) DO UPDATE SET payload = EXCLUDED.payload, n = app_db_test.n + 1;";
const GET: &str =
    "-- ironcrew:op\n-- params: key text\nSELECT key, payload, n FROM app_db_test WHERE key = $1;";
const ALL: &str = "-- ironcrew:op\nSELECT key FROM app_db_test;";

#[tokio::test]
async fn upsert_query_and_query_one_round_trip() {
    let Some(url) = test_url() else { return };
    let db = app(
        &url,
        vec![
            ("setup", SETUP),
            ("teardown", TEARDOWN),
            ("upsert", UPSERT),
            ("get", GET),
            ("all", ALL),
        ],
    );
    db.execute("setup", &[]).await.unwrap();

    let payload = serde_json::json!({"stage": "classification", "score": 0.9});
    let key = serde_json::json!("run-1:classification");
    assert_eq!(
        db.execute("upsert", &[key.clone(), payload.clone()])
            .await
            .unwrap(),
        1
    );
    // Idempotent re-run: same key upserts rather than duplicating.
    assert_eq!(
        db.execute("upsert", &[key.clone(), payload.clone()])
            .await
            .unwrap(),
        1
    );

    let row = db
        .query_one("get", &[key])
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(row["payload"]["stage"], "classification");
    assert_eq!(row["n"], 1, "second upsert hit the conflict branch");
    assert!(
        db.query_one("get", &[serde_json::json!("absent")])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(db.query("all", &[]).await.unwrap().len(), 1);

    db.execute("teardown", &[]).await.unwrap();
}

#[tokio::test]
async fn query_one_with_multiple_matches_is_an_error() {
    let Some(url) = test_url() else { return };
    let db = app(
        &url,
        vec![(
            "multi",
            "-- ironcrew:op\nSELECT generate_series(1, 2) AS n;",
        )],
    );
    let err = db.query_one("multi", &[]).await.unwrap_err().to_string();
    assert!(err.contains("more than one row"), "{err}");
}

#[tokio::test]
async fn multi_statement_execute_is_atomic() {
    let Some(url) = test_url() else { return };
    let db = app(
        &url,
        vec![
            (
                "setup",
                "-- ironcrew:op\nCREATE TABLE IF NOT EXISTS app_db_atomic (k text PRIMARY KEY);",
            ),
            (
                "teardown",
                "-- ironcrew:op\nDROP TABLE IF EXISTS app_db_atomic;",
            ),
            // Statement 2 violates the PK inserted by statement 1 → whole op rolls back.
            (
                "both",
                "-- ironcrew:op\n-- params: k text\nINSERT INTO app_db_atomic (k) VALUES ($1);\nINSERT INTO app_db_atomic (k) VALUES ($1);",
            ),
            (
                "count",
                "-- ironcrew:op\nSELECT count(*)::int8 AS c FROM app_db_atomic;",
            ),
        ],
    );
    db.execute("setup", &[]).await.unwrap();
    db.execute("both", &[serde_json::json!("x")])
        .await
        .unwrap_err();
    let row = db.query_one("count", &[]).await.unwrap().unwrap();
    assert_eq!(row["c"], 0, "partial insert must be rolled back");
    db.execute("teardown", &[]).await.unwrap();
}

#[tokio::test]
async fn row_byte_and_time_limits_are_enforced() {
    let Some(url) = test_url() else { return };
    let db = app(
        &url,
        vec![
            (
                "many",
                "-- ironcrew:op\nSELECT generate_series(1, 501) AS n;",
            ),
            (
                "big",
                "-- ironcrew:op\nSELECT repeat('x', 2 * 1024 * 1024) AS blob;",
            ),
            ("slow", "-- ironcrew:op\nSELECT pg_sleep(10);"),
            ("weird", "-- ironcrew:op\nSELECT now() AS ts;"),
        ],
    );
    let err = db.query("many", &[]).await.unwrap_err().to_string();
    assert!(err.contains("IRONCREW_APP_DB_MAX_ROWS"), "{err}");
    let err = db.query("big", &[]).await.unwrap_err().to_string();
    assert!(err.contains("IRONCREW_APP_DB_MAX_RESPONSE_BYTES"), "{err}");
    let err = db.query("slow", &[]).await.unwrap_err().to_string();
    assert!(err.to_lowercase().contains("statement timeout"), "{err}");
    // Unsupported column type names the cast escape hatch.
    let err = db.query("weird", &[]).await.unwrap_err().to_string();
    assert!(err.contains("::text"), "{err}");
}

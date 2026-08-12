use sqlx::{Connection, PgConnection, PgPool};

use super::*;

type JournalStateRow = (i64, i64, i64, i64, bool, Option<String>, Option<i64>);

#[derive(Debug, Clone, PartialEq)]
pub(super) struct JournalRow {
    pub(super) sequence: u64,
    pub(super) event_type: String,
    pub(super) payload: serde_json::Value,
    pub(super) payload_bytes: u64,
    pub(super) accounted_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct JournalSnapshot {
    pub(super) latest_sequence: u64,
    pub(super) dropped_through: u64,
    pub(super) retained_events: u64,
    pub(super) retained_bytes: u64,
    pub(super) journal_complete: bool,
    pub(super) eviction_reason: Option<String>,
    pub(super) terminal_event_sequence: Option<u64>,
    pub(super) rows: Vec<JournalRow>,
    pub(super) global_events: u64,
    pub(super) global_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct SseFrame {
    pub(super) id: Option<String>,
    pub(super) event: String,
    pub(super) data: serde_json::Value,
}

fn nonnegative(label: &str, value: i64) -> u64 {
    u64::try_from(value).unwrap_or_else(|_| panic!("{label} is negative: {value}"))
}

async fn journal_snapshot(pool: &PgPool, prefix: &str, run_id: &str) -> Option<JournalSnapshot> {
    let state_sql = format!(
        "SELECT latest_sequence, dropped_through, retained_events, retained_bytes, \
                journal_complete, eviction_reason, terminal_event_sequence \
         FROM {prefix}run_event_state WHERE run_id=$1"
    );
    let state: Option<JournalStateRow> = sqlx::query_as(sqlx::AssertSqlSafe(state_sql))
        .bind(run_id)
        .fetch_optional(pool)
        .await
        .expect("read IC-017 journal state");
    let state = state?;
    let rows_sql = format!(
        "SELECT sequence, event_type, payload::text, payload_bytes, accounted_bytes \
         FROM {prefix}run_events WHERE run_id=$1 ORDER BY sequence"
    );
    let rows: Vec<(i64, String, String, i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(rows_sql))
        .bind(run_id)
        .fetch_all(pool)
        .await
        .expect("read IC-017 journal rows");
    let usage_sql = format!(
        "SELECT retained_events, retained_bytes FROM {prefix}run_event_usage WHERE singleton=TRUE"
    );
    let usage: (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(usage_sql))
        .fetch_one(pool)
        .await
        .expect("read IC-017 journal usage");

    Some(JournalSnapshot {
        latest_sequence: nonnegative("latest sequence", state.0),
        dropped_through: nonnegative("dropped boundary", state.1),
        retained_events: nonnegative("retained event count", state.2),
        retained_bytes: nonnegative("retained event bytes", state.3),
        journal_complete: state.4,
        eviction_reason: state.5,
        terminal_event_sequence: state
            .6
            .map(|value| nonnegative("terminal event sequence", value)),
        rows: rows
            .into_iter()
            .map(
                |(sequence, event_type, payload, payload_bytes, accounted_bytes)| JournalRow {
                    sequence: nonnegative("event sequence", sequence),
                    event_type,
                    payload: serde_json::from_str(&payload).expect("parse IC-017 event payload"),
                    payload_bytes: nonnegative("event payload bytes", payload_bytes),
                    accounted_bytes: nonnegative("event accounted bytes", accounted_bytes),
                },
            )
            .collect(),
        global_events: nonnegative("global event count", usage.0),
        global_bytes: nonnegative("global event bytes", usage.1),
    })
}

pub(super) async fn wait_for_journal<F>(
    pool: &PgPool,
    prefix: &str,
    run_id: &str,
    description: &str,
    ready: F,
) -> JournalSnapshot
where
    F: Fn(&JournalSnapshot) -> bool,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = None;
    while Instant::now() < deadline {
        if let Some(observed) = journal_snapshot(pool, prefix, run_id).await {
            if ready(&observed) {
                return observed;
            }
            last = Some(observed);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("journal did not reach {description}; last snapshot: {last:#?}");
}

pub(super) async fn lock_journal_reads(
    database_url: &str,
    prefix: &str,
    run_id: &str,
) -> PgConnection {
    let mut connection = PgConnection::connect(database_url)
        .await
        .expect("connect IC-017 journal read barrier");
    sqlx::query("BEGIN")
        .execute(&mut connection)
        .await
        .expect("begin IC-017 journal read barrier");
    let sql = format!("SELECT run_id FROM {prefix}run_event_state WHERE run_id=$1 FOR UPDATE");
    let locked: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .fetch_one(&mut connection)
        .await
        .expect("lock IC-017 journal state row");
    assert_eq!(locked, run_id);
    connection
}

pub(super) async fn unlock_journal_reads(mut connection: PgConnection) {
    sqlx::query("ROLLBACK")
        .execute(&mut connection)
        .await
        .expect("release IC-017 journal read barrier");
    connection
        .close()
        .await
        .expect("close IC-017 journal read barrier");
}

pub(super) async fn expire_journal_rows(
    pool: &PgPool,
    prefix: &str,
    run_id: &str,
    expected_rows: u64,
) {
    let sql = format!(
        "UPDATE {prefix}run_events \
         SET created_at=clock_timestamp() - interval '2 seconds', \
             expires_at=clock_timestamp() - interval '1 second' \
         WHERE run_id=$1"
    );
    let changed = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .execute(pool)
        .await
        .expect("expire IC-017 journal rows")
        .rows_affected();
    assert_eq!(changed, expected_rows);
}

pub(super) fn parse_sse_frames(body: &str) -> Vec<SseFrame> {
    let normalized = body.replace("\r\n", "\n");
    let Some(complete_end) = normalized.rfind("\n\n") else {
        return Vec::new();
    };
    normalized[..complete_end]
        .split("\n\n")
        .filter_map(|raw| {
            let mut id = None;
            let mut event = None;
            let mut data = Vec::new();
            for line in raw.lines() {
                if let Some(value) = line.strip_prefix("id:") {
                    id = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data.push(value.trim_start());
                }
            }
            let event = event?;
            let payload = data.join("\n");
            Some(SseFrame {
                id,
                event,
                data: serde_json::from_str(&payload).expect("parse IC-017 SSE payload"),
            })
        })
        .collect()
}

pub(super) async fn read_until_sse_event(response: &mut Response, event: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut body = String::new();
        loop {
            let chunk = response
                .chunk()
                .await
                .expect("read IC-017 SSE chunk")
                .expect("IC-017 SSE ended before expected event");
            body.push_str(&String::from_utf8_lossy(&chunk));
            if parse_sse_frames(&body)
                .iter()
                .any(|frame| frame.event == event)
            {
                return body;
            }
        }
    })
    .await
    .expect("timed out waiting for IC-017 SSE event")
}

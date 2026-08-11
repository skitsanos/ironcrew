use sqlx::Row;

use super::*;

#[derive(Clone, PartialEq)]
pub(super) struct MailboxSnapshot {
    pub(super) question_id: String,
    state: String,
    question_fingerprint: String,
    question_nonce: Vec<u8>,
    question_ciphertext: Vec<u8>,
    answer_fingerprint: Option<String>,
    answer_nonce: Option<Vec<u8>>,
    answer_ciphertext: Option<Vec<u8>>,
}

async fn mailbox(pair: &ProcessPair, run_id: &str) -> Option<MailboxSnapshot> {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-016 mailbox evidence");
    let sql = format!(
        "SELECT question_id, state, question_key_fingerprint, question_nonce, \
                question_ciphertext, answer_key_fingerprint, answer_nonce, answer_ciphertext \
         FROM {p}human_inputs WHERE run_id=$1 ORDER BY question_id LIMIT 2",
        p = pair.prefix
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .fetch_all(&pool)
        .await
        .expect("read IC-016 mailbox row");
    pool.close().await;
    assert!(rows.len() <= 1, "IC-016 run retained multiple questions");
    rows.into_iter().next().map(|row| MailboxSnapshot {
        question_id: row.get("question_id"),
        state: row.get("state"),
        question_fingerprint: row.get("question_key_fingerprint"),
        question_nonce: row.get("question_nonce"),
        question_ciphertext: row.get("question_ciphertext"),
        answer_fingerprint: row.get("answer_key_fingerprint"),
        answer_nonce: row.get("answer_nonce"),
        answer_ciphertext: row.get("answer_ciphertext"),
    })
}

pub(super) async fn wait_for_pending(
    pair: &ProcessPair,
    run_id: &str,
    expected_fingerprint: &str,
    prompt: &str,
) -> MailboxSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(row) = mailbox(pair, run_id).await {
            assert_eq!(row.state, "pending");
            assert_eq!(row.question_fingerprint, expected_fingerprint);
            assert_eq!(row.question_nonce.len(), 12);
            assert!(row.question_ciphertext.len() > prompt.len());
            assert!(!contains(&row.question_ciphertext, prompt.as_bytes()));
            assert!(row.answer_fingerprint.is_none());
            assert!(row.answer_nonce.is_none());
            assert!(row.answer_ciphertext.is_none());
            return row;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("IC-016 mailbox did not retain one pending question");
}

pub(super) async fn assert_unchanged(pair: &ProcessPair, run_id: &str, expected: &MailboxSnapshot) {
    let observed = mailbox(pair, run_id)
        .await
        .expect("IC-016 early-removal probe deleted the mailbox row");
    assert!(
        observed == *expected,
        "IC-016 early-removal probe mutated encrypted mailbox state"
    );
}

pub(super) async fn assert_answered(
    pair: &ProcessPair,
    run_id: &str,
    expected_question_fingerprint: &str,
    expected_answer_fingerprint: &str,
    prompt: &str,
    answer: &str,
) {
    let row = mailbox(pair, run_id)
        .await
        .expect("IC-016 answered mailbox row");
    assert_eq!(row.state, "answered");
    assert_eq!(row.question_fingerprint, expected_question_fingerprint);
    assert_eq!(
        row.answer_fingerprint.as_deref(),
        Some(expected_answer_fingerprint)
    );
    assert_eq!(row.question_nonce.len(), 12);
    let answer_nonce = row.answer_nonce.expect("IC-016 answer nonce");
    let answer_ciphertext = row.answer_ciphertext.expect("IC-016 answer ciphertext");
    assert_eq!(answer_nonce.len(), 12);
    assert!(answer_ciphertext.len() > answer.len());
    assert!(!contains(&row.question_ciphertext, prompt.as_bytes()));
    assert!(!contains(&answer_ciphertext, answer.as_bytes()));
}

pub(super) async fn assert_no_old_references(pair: &ProcessPair, old_fingerprint: &str) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-016 fingerprint evidence");
    let sql = format!(
        "SELECT COUNT(*) FROM {p}human_inputs \
         WHERE question_key_fingerprint=$1 OR answer_key_fingerprint=$1",
        p = pair.prefix
    );
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(old_fingerprint)
        .fetch_one(&pool)
        .await
        .expect("count IC-016 old-key references");
    pool.close().await;
    assert_eq!(count, 0, "IC-016 old-key references remain");
}

pub(super) async fn assert_consumed_once(pair: &ProcessPair, run_id: &str) {
    wait_for_status(pair, run_id, "Success").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let pool = sqlx::PgPool::connect(&pair.database_url)
            .await
            .expect("connect for IC-016 consumption evidence");
        let sql = format!(
            "SELECT \
             (SELECT COUNT(*) FROM {p}human_inputs WHERE run_id=$1) mailbox, \
             (SELECT COUNT(*) FROM {p}run_events WHERE run_id=$1 \
                AND event_type='human_input_requested') requested, \
             (SELECT COUNT(*) FROM {p}run_events WHERE run_id=$1 \
                AND event_type='human_input_received') received, \
             (SELECT COUNT(*) FROM {p}run_events WHERE run_id=$1 \
                AND event_type='run_complete') completed, \
             (SELECT COUNT(*) FROM {p}audit_events WHERE target=$1 \
                AND action='flow.run.question_answer' AND success=TRUE \
                AND status_code=202) accepted",
            p = pair.prefix
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("read IC-016 consumption evidence");
        pool.close().await;
        let counts: (i64, i64, i64, i64, i64) = (
            row.get("mailbox"),
            row.get("requested"),
            row.get("received"),
            row.get("completed"),
            row.get("accepted"),
        );
        if counts == (0, 1, 1, 1, 1) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("IC-016 answer was not consumed and completed exactly once");
}

pub(super) async fn assert_shared_sql_hides(pair: &ProcessPair, run_id: &str, forbidden: &[&str]) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-016 SQL privacy evidence");
    let sql = format!(
        "SELECT \
         COALESCE((SELECT string_agg(payload::text, ' ') FROM {p}run_events \
                   WHERE run_id=$1), '') events, \
         COALESCE((SELECT string_agg(COALESCE(metadata::text, ''), ' ') \
                   FROM {p}audit_events WHERE target=$1), '') audits, \
         COALESCE((SELECT row_to_json(run)::text FROM {p}runs run \
                   WHERE run_id=$1), '') run",
        p = pair.prefix
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read IC-016 shared SQL payloads");
    pool.close().await;
    let surfaces = [
        row.get::<String, _>("events"),
        row.get::<String, _>("audits"),
        row.get::<String, _>("run"),
    ];
    for value in forbidden {
        assert!(
            surfaces.iter().all(|surface| !surface.contains(value)),
            "IC-016 shared SQL exposed a forbidden plaintext value"
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|part| part == needle)
}

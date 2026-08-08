use futures::FutureExt;
use reqwest::StatusCode;
use sqlx::Row;

use super::*;

const FIXTURE: &str = include_str!("../../fixtures/two_process_replica/unkeyed_post_crew.lua");
const PROMPT: &str = "Keep the unkeyed owner alive?";

async fn terminal_details(pair: &ProcessPair, run_id: &str) -> (i64, i64, serde_json::Value) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-006 terminal details");
    let sql = format!(
        "SELECT state.latest_sequence, state.terminal_event_sequence, event.payload::text payload \
         FROM {p}run_event_state state JOIN {p}run_events event \
           ON event.run_id=state.run_id AND event.sequence=state.terminal_event_sequence \
         WHERE state.run_id=$1",
        p = pair.prefix
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read IC-006 terminal details");
    let payload: String = row.get("payload");
    let details = (
        row.get("latest_sequence"),
        row.get("terminal_event_sequence"),
        serde_json::from_str(&payload).expect("parse stored terminal event payload"),
    );
    pool.close().await;
    details
}

async fn audit_rows(pair: &ProcessPair, run_id: &str) -> Vec<(i32, bool, Option<String>, String)> {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for IC-006 audit evidence");
    let sql = format!(
        "SELECT status_code, success, actor, flow_path FROM {p}audit_events \
         WHERE action='flow.run.abort' AND target=$1 ORDER BY timestamp",
        p = pair.prefix
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(run_id)
        .fetch_all(&pool)
        .await
        .expect("read IC-006 abort audits")
        .into_iter()
        .map(|row| {
            (
                row.get("status_code"),
                row.get("success"),
                row.get("actor"),
                row.get("flow_path"),
            )
        })
        .collect();
    pool.close().await;
    rows
}

async fn run_through(pair: &ProcessPair, base_url: &str, run_id: &str) -> serde_json::Value {
    authenticated(
        pair.client
            .get(format!("{base_url}/flows/{FLOW}/runs/{run_id}")),
    )
    .send()
    .await
    .expect("read IC-006 shared run history")
    .error_for_status()
    .expect("IC-006 run-history response")
    .json()
    .await
    .expect("parse IC-006 run history")
}

async fn local_question(pair: &ProcessPair, run_id: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let response = authenticated(pair.client.get(format!(
            "{}/flows/{FLOW}/questions/{run_id}",
            pair.owner_a.base_url
        )))
        .send()
        .await
        .expect("poll IC-006 owner-local question");
        if response.status() == StatusCode::OK {
            let body: serde_json::Value = response.json().await.expect("parse local question");
            if body["questions"]
                .as_array()
                .is_some_and(|questions| questions.iter().any(|item| item["prompt"] == PROMPT))
            {
                assert_eq!(body["status"], "waiting_for_input");
                assert_eq!(body["owner_instance_id"], pair.owner_a_id);
                assert_eq!(body["control_scope"], "process");
                return body;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("IC-006 owner-local question did not appear");
}

async fn peer_abort(pair: &ProcessPair, run_id: &str) -> (StatusCode, serde_json::Value) {
    let response = authenticated(pair.client.post(format!(
        "{}/flows/{FLOW}/abort/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("abort IC-006 run through non-owner");
    let status = response.status();
    let body = response.json().await.expect("parse IC-006 abort response");
    (status, body)
}

fn without_lease_or_audit(mut value: ScopedSnapshot) -> ScopedSnapshot {
    value.run_lease = None;
    value.abort_audits = 0;
    value.valid_abort_audits = 0;
    value
}

async fn scenario(pair: &mut ProcessPair) {
    let started = authenticated(
        pair.client
            .post(format!("{}/flows/{FLOW}/run", pair.owner_a.base_url)),
    )
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("start IC-006 unkeyed run");
    assert_eq!(started.status(), StatusCode::OK);
    assert!(started.headers().get("Idempotency-Replayed").is_none());
    let acceptance: serde_json::Value = started.json().await.expect("parse IC-006 acceptance");
    assert_eq!(acceptance["status"], "started");
    assert_eq!(acceptance["owner_instance_id"], pair.owner_a_id);
    assert_eq!(acceptance["control_scope"], "process");
    let run_id = acceptance["run_id"]
        .as_str()
        .expect("IC-006 run id")
        .to_string();

    let first_question = local_question(pair, &run_id).await;
    let question_id = first_question["questions"][0]["question_id"]
        .as_str()
        .expect("IC-006 question id")
        .to_string();

    let mut peer_events = authenticated(pair.client.get(format!(
        "{}/flows/{FLOW}/events/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("read IC-006 durable events through peer");
    assert_eq!(peer_events.status(), StatusCode::OK);
    let event_body = read_sse_until(&mut peer_events, "event: human_input_requested").await;
    assert!(event_body.contains("omitted_from_event_journal"));
    assert!(!event_body.contains(PROMPT));
    drop(peer_events);

    let history_a = run_through(pair, &pair.owner_a.base_url, &run_id).await;
    let history_b = run_through(pair, &pair.survivor_b.base_url, &run_id).await;
    for history in [&history_a, &history_b] {
        assert_eq!(history["run_id"], run_id);
        assert_eq!(history["status"], "WaitingForInput");
        assert_eq!(history["owner_instance_id"], pair.owner_a_id);
        assert_eq!(history["task_results"], serde_json::json!([]));
        chrono::DateTime::parse_from_rfc3339(
            history["lease_expires_at"].as_str().expect("history lease"),
        )
        .expect("parse history lease");
    }

    let baseline = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let observed = snapshot(pair, &run_id).await;
            if observed.human_requested == 1 {
                break observed;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("durable human-input event did not reach the IC-006 baseline");
    assert_eq!(baseline.runs, 1);
    assert_eq!(baseline.status.as_deref(), Some("waiting_for_input"));
    assert_eq!(
        baseline.run_owner.as_deref(),
        Some(pair.owner_a_id.as_str())
    );
    assert_eq!(baseline.ledgers, 0);
    assert_eq!(baseline.mailbox, 0);
    assert_eq!(baseline.human_requested, 1);
    assert_eq!(baseline.run_complete, 0);
    assert_eq!(baseline.journal_complete, Some(true));
    assert_eq!(baseline.terminal_sequence, None);
    let (wrong_owner_status, wrong_owner) = peer_abort(pair, &run_id).await;
    assert_eq!(wrong_owner_status, StatusCode::CONFLICT);
    assert_eq!(
        wrong_owner,
        serde_json::json!({
            "error": "Run is active on another IronCrew instance",
            "code": "run_owned_by_another_instance",
            "run_id": run_id,
            "owner_instance_id": pair.owner_a_id,
            "control_scope": "process",
            "retryable": true,
        })
    );
    let after_conflict = snapshot(pair, &run_id).await;
    assert_eq!(
        without_lease_or_audit(after_conflict.clone()),
        without_lease_or_audit(baseline.clone()),
        "wrong-owner control changed durable run or event state"
    );
    let question_after_conflict = local_question(pair, &run_id).await;
    assert_eq!(
        question_after_conflict["questions"][0]["question_id"],
        question_id
    );
    let lease_before = chrono::DateTime::parse_from_rfc3339(
        after_conflict
            .run_lease
            .as_deref()
            .expect("IC-006 active lease"),
    )
    .expect("parse IC-006 active lease");
    wait_for_next_lease(pair, &run_id).await;
    let renewed = snapshot(pair, &run_id).await;
    let lease_after = chrono::DateTime::parse_from_rfc3339(
        renewed.run_lease.as_deref().expect("IC-006 renewed lease"),
    )
    .expect("parse IC-006 renewed lease");
    assert!(lease_after > lease_before);
    assert_eq!(
        audit_rows(pair, &run_id).await,
        vec![(409, false, Some("acceptance-client".into()), FLOW.into())]
    );

    let owner_status = pair.owner_a.shutdown();
    assert!(
        owner_status.success(),
        "IC-006 owner did not drain cleanly: {}",
        pair.owner_a.logs()
    );
    let terminal = wait_for_status(pair, &run_id, "Aborted").await;
    assert_eq!(terminal["owner_instance_id"], pair.owner_a_id);
    assert_eq!(terminal["lease_expires_at"], "");
    assert_eq!(terminal["total_tokens"], 0);
    assert_eq!(terminal["task_results"].as_array().map(Vec::len), Some(1));
    assert_eq!(terminal["task_results"][0]["task"], "skipped");
    assert_eq!(terminal["task_results"][0]["success"], true);
    assert_ready(pair).await;

    let final_state = snapshot(pair, &run_id).await;
    assert_eq!(final_state.runs, 1);
    assert_eq!(final_state.status.as_deref(), Some("aborted"));
    assert_eq!(
        final_state.run_owner.as_deref(),
        Some(pair.owner_a_id.as_str())
    );
    assert_eq!(final_state.run_lease.as_deref(), Some(""));
    assert!(
        final_state
            .run_finished
            .as_deref()
            .is_some_and(|finished| !finished.is_empty())
    );
    assert_eq!(final_state.ledgers, 0);
    assert_eq!(final_state.mailbox, 0);
    assert_eq!(final_state.events, baseline.events + 1);
    assert_eq!(final_state.human_requested, 1);
    assert_eq!(final_state.run_complete, 1);
    assert_eq!(final_state.journal_complete, Some(true));
    assert!(final_state.terminal_sequence.is_some());
    let (latest_sequence, terminal_sequence, terminal_payload) =
        terminal_details(pair, &run_id).await;
    assert_eq!(latest_sequence, terminal_sequence);
    assert_eq!(final_state.terminal_sequence, Some(terminal_sequence));
    assert_eq!(terminal_payload["data"]["status"], "aborted");

    let (terminal_status, terminal_abort) = peer_abort(pair, &run_id).await;
    assert_eq!(terminal_status, StatusCode::NOT_FOUND);
    assert_eq!(
        terminal_abort,
        serde_json::json!({
            "error": format!("Run '{run_id}' not found or already completed")
        })
    );
    assert_eq!(
        audit_rows(pair, &run_id).await,
        vec![
            (409, false, Some("acceptance-client".into()), FLOW.into()),
            (404, false, Some("acceptance-client".into()), FLOW.into()),
        ]
    );
    let after_terminal_abort = snapshot(pair, &run_id).await;
    assert_eq!(
        without_lease_or_audit(after_terminal_abort),
        without_lease_or_audit(final_state),
        "already-terminal abort changed authoritative durable state"
    );
}

#[tokio::test]
async fn ic006_unkeyed_wrong_owner_is_truthful_across_processes() {
    with_process_pair("ic006", false, FIXTURE, |pair| scenario(pair).boxed_local()).await;
}

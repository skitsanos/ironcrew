use futures::FutureExt;
use reqwest::StatusCode;
use sqlx::Row;

use super::*;

#[path = "../../support/scripted_hitl_provider.rs"]
mod scripted_hitl_provider;

use scripted_hitl_provider::{
    ANALYST_ANSWER, ANALYST_PROMPT, REVIEWER_ANSWER, REVIEWER_PROMPT, ScriptedHitlProbe,
    ScriptedHitlProvider,
};

const FIXTURE: &str = include_str!("../../fixtures/two_process_replica/hitl_agents_crew.lua");
const IDEMPOTENCY_KEY: &str = "hitl-agent-replica-acceptance-0001";

async fn answer(pair: &ProcessPair, run_id: &str, question_id: &str, value: &str) {
    let response = authenticated(pair.client.post(format!(
        "{}/flows/{FLOW}/answer/{run_id}",
        pair.survivor_b.base_url
    )))
    .json(&serde_json::json!({
        "question_id": question_id,
        "answer": value,
    }))
    .send()
    .await
    .expect("answer named agent through peer replica");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let body = response.text().await.expect("read peer answer response");
    assert!(!body.contains(value), "answer leaked in HTTP response");
}

async fn assert_durable_event_counts(pair: &ProcessPair, run_id: &str) {
    let pool = sqlx::PgPool::connect(&pair.database_url)
        .await
        .expect("connect for named-agent HITL evidence");
    let statement = format!(
        "SELECT event_type, payload::text AS payload FROM {}run_events \
         WHERE run_id=$1 AND event_type IN ('human_input_requested','human_input_received') \
         ORDER BY sequence",
        pair.prefix
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(run_id)
        .fetch_all(&pool)
        .await
        .expect("read named-agent HITL events");
    pool.close().await;
    assert_eq!(rows.len(), 4);
    let mut requested = 0;
    let mut received = 0;
    for row in rows {
        let event_type: String = row.get("event_type");
        let payload: String = row.get("payload");
        assert!(!payload.contains(ANALYST_ANSWER));
        assert!(!payload.contains(REVIEWER_ANSWER));
        match event_type.as_str() {
            "human_input_requested" => requested += 1,
            "human_input_received" => received += 1,
            _ => unreachable!("query returned an unexpected event type"),
        }
    }
    assert_eq!((requested, received), (2, 2));
}

async fn scenario(pair: &mut ProcessPair, provider: ScriptedHitlProbe) {
    let started = start_keyed_run(pair, IDEMPOTENCY_KEY).await;
    let run_id = started["run_id"]
        .as_str()
        .expect("named-agent HITL run id")
        .to_string();
    assert_eq!(started["owner_instance_id"], pair.owner_a_id);

    let analyst = wait_for_shared_question(pair, &run_id, ANALYST_PROMPT).await;
    let analyst_id = analyst["question_id"]
        .as_str()
        .expect("analyst question id");
    answer(pair, &run_id, analyst_id, ANALYST_ANSWER).await;

    let reviewer = wait_for_shared_question(pair, &run_id, REVIEWER_PROMPT).await;
    let reviewer_id = reviewer["question_id"]
        .as_str()
        .expect("reviewer question id");
    assert_ne!(analyst_id, reviewer_id);
    answer(pair, &run_id, reviewer_id, REVIEWER_ANSWER).await;

    let terminal = wait_for_status(pair, &run_id, "Success").await;
    assert_eq!(terminal["owner_instance_id"], pair.owner_a_id);
    let results = terminal["task_results"]
        .as_array()
        .expect("named-agent task results");
    assert_eq!(results.len(), 2);
    let analyst = results
        .iter()
        .find(|result| result["task"] == "analyze")
        .expect("analyst task result");
    assert_eq!(analyst["agent"], "analyst");
    assert_eq!(analyst["output"], "FINAL:dataset-alpha");
    let reviewer = results
        .iter()
        .find(|result| result["task"] == "review")
        .expect("reviewer task result");
    assert_eq!(reviewer["agent"], "reviewer");
    assert_eq!(reviewer["output"], "FINAL:approved");

    let events = authenticated(pair.client.get(format!(
        "{}/flows/{FLOW}/events/{run_id}",
        pair.survivor_b.base_url
    )))
    .send()
    .await
    .expect("read named-agent HITL replay")
    .error_for_status()
    .expect("named-agent HITL replay response")
    .text()
    .await
    .expect("read named-agent HITL replay body");
    assert_eq!(events.matches("event: human_input_requested").count(), 2);
    assert_eq!(events.matches("event: human_input_received").count(), 2);
    let hitl_frames = events
        .split("\n\n")
        .filter(|frame| {
            frame.lines().any(|line| {
                line == "event: human_input_requested" || line == "event: human_input_received"
            })
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    assert!(!hitl_frames.contains(ANALYST_ANSWER));
    assert!(!hitl_frames.contains(REVIEWER_ANSWER));

    assert_durable_event_counts(pair, &run_id).await;
    provider.assert_complete();
}

#[tokio::test]
async fn named_agents_can_talk_to_a_human_through_postgres_replicas() {
    let provider = ScriptedHitlProvider::start();
    let probe = provider.probe();
    let env = [
        ("IRONCREW_ALLOW_PRIVATE_IPS", "1"),
        ("IRONCREW_ENV_ALLOWLIST", "HITL_PROVIDER_BASE_URL"),
        ("HITL_PROVIDER_BASE_URL", probe.base_url.as_str()),
    ];
    with_configured_process_pair("hitl-agents", true, FIXTURE, &env, |pair| {
        let probe = probe.clone();
        async move { scenario(pair, probe).await }.boxed_local()
    })
    .await;
}

use super::*;

const FIXTURE: &str = include_str!("../../fixtures/two_process_replica/crew.lua");
const PRE_PICKUP_KEY: &str = "ic005-owner-death-before-cancellation-pickup-0001";
const POST_PICKUP_KEY: &str = "ic005-owner-death-after-cancellation-pickup-0002";
const PICKUP_LOG: &str = "Run worker stopped after a durable cancellation request";

async fn stable_waiting_snapshot(pair: &ProcessPair, run_id: &str) -> ScopedSnapshot {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let observed = snapshot(pair, run_id).await;
        if observed.status.as_deref() == Some("waiting_for_input")
            && observed.ledger_state.as_deref() == Some("running")
            && observed.mailbox == 1
            && observed.events == 1
            && observed.human_requested == 1
        {
            assert_eq!(observed.runs, 1);
            assert_eq!(
                observed.run_owner.as_deref(),
                Some(pair.owner_a_id.as_str())
            );
            assert_eq!(observed.ledgers, 1);
            assert_eq!(
                observed.ledger_owner.as_deref(),
                Some(pair.owner_a_id.as_str())
            );
            assert_eq!(observed.response_status, Some(200));
            assert_eq!(observed.run_complete, 0);
            assert_eq!(observed.journal_complete, Some(true));
            assert_eq!(observed.terminal_sequence, None);
            assert_eq!(observed.abort_audits, 0);
            return observed;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run never reached the stable pre-cancellation snapshot");
}

fn assert_cancel_ack(body: &serde_json::Value, pair: &ProcessPair, run_id: &str) {
    assert_eq!(body["run_id"], run_id);
    assert_eq!(body["status"], "cancellation_requested");
    assert_eq!(body["owner_instance_id"], pair.owner_a_id);
    assert_eq!(body["control_scope"], "shared_store");
    assert_eq!(body["already_requested"], false);
}

fn assert_cancel_pending(
    observed: &ScopedSnapshot,
    baseline: &ScopedSnapshot,
    pair: &ProcessPair,
    expected_status: &str,
) {
    assert_eq!(observed.runs, 1);
    assert_eq!(observed.status.as_deref(), Some(expected_status));
    assert_eq!(observed.run_owner, baseline.run_owner);
    assert_eq!(observed.ledgers, 1);
    assert_eq!(observed.ledger_state.as_deref(), Some("running"));
    assert_eq!(
        observed.ledger_owner.as_deref(),
        Some(pair.owner_a_id.as_str())
    );
    assert!(observed.cancel_requested);
    assert_eq!(observed.response_status, Some(200));
    assert_eq!(observed.response_body, baseline.response_body);
    assert_eq!(observed.mailbox, 0);
    assert_eq!(observed.events, baseline.events);
    assert_eq!(observed.human_requested, 1);
    assert_eq!(observed.run_complete, 0);
    assert_eq!(observed.journal_complete, Some(true));
    assert_eq!(observed.terminal_sequence, None);
    assert_eq!(observed.abort_audits, 1);
    assert_eq!(observed.valid_abort_audits, 1);
}

fn assert_abandoned(observed: &ScopedSnapshot, baseline: &ScopedSnapshot, pair: &ProcessPair) {
    assert_eq!(observed.runs, 1);
    assert_eq!(observed.status.as_deref(), Some("abandoned"));
    assert_eq!(
        observed.run_owner.as_deref(),
        Some(pair.owner_a_id.as_str())
    );
    assert_eq!(observed.run_lease.as_deref(), Some(""));
    assert!(
        observed
            .run_finished
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(observed.ledgers, 1);
    assert_eq!(observed.ledger_state.as_deref(), Some("completed"));
    assert_eq!(
        observed.ledger_owner.as_deref(),
        Some(pair.owner_a_id.as_str())
    );
    assert_eq!(observed.ledger_lease.as_deref(), Some(""));
    assert!(observed.cancel_requested);
    assert_eq!(observed.response_status, Some(200));
    assert_eq!(observed.response_body, baseline.response_body);
    assert!(observed.ledger_completed.is_some());
    assert!(observed.ledger_expires.is_some());
    assert_eq!(observed.mailbox, 0);
    assert_eq!(observed.events, baseline.events);
    assert_eq!(observed.human_requested, 1);
    assert_eq!(observed.run_complete, 0);
    assert_eq!(observed.journal_complete, Some(false));
    assert_eq!(observed.terminal_sequence, None);
    assert_eq!(observed.abort_audits, 1);
    assert_eq!(observed.valid_abort_audits, 1);
}

async fn verify_final_fences(
    pair: &ProcessPair,
    key: &str,
    run_id: &str,
    baseline: &ScopedSnapshot,
    started: &serde_json::Value,
) {
    let terminal = wait_for_status(pair, run_id, "Abandoned").await;
    assert_eq!(terminal["owner_instance_id"], pair.owner_a_id);
    assert_eq!(terminal["lease_expires_at"], "");
    let final_snapshot = snapshot(pair, run_id).await;
    assert_abandoned(&final_snapshot, baseline, pair);
    let first_fallback = synthesized_abandoned_frame(pair, run_id).await;
    let second_fallback = synthesized_abandoned_frame(pair, run_id).await;
    assert_eq!(first_fallback, second_fallback);

    tokio::join!(
        assert_replay(pair, key, run_id, started),
        assert_replay(pair, key, run_id, started),
        assert_replay(pair, key, run_id, started),
    );
    assert_eq!(snapshot(pair, run_id).await, final_snapshot);
    assert_stale_completion_fenced(pair, run_id).await;
    assert_eq!(snapshot(pair, run_id).await, final_snapshot);
    assert_ready(pair).await;
}

#[tokio::test]
async fn owner_death_before_durable_cancellation_pickup_reconciles_once() {
    with_process_pair("ic005-pre", true, FIXTURE, |pair| {
        Box::pin(async move {
            let started = start_keyed_run(pair, PRE_PICKUP_KEY).await;
            let run_id = started["run_id"].as_str().expect("run id").to_string();
            assert_eq!(started["owner_instance_id"], pair.owner_a_id);
            assert_eq!(started["control_scope"], "process");
            wait_for_shared_question(pair, &run_id, FIRST_PROMPT).await;
            let baseline = stable_waiting_snapshot(pair, &run_id).await;
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(
                    baseline
                        .response_body
                        .as_deref()
                        .expect("retained response")
                )
                .expect("parse retained response"),
                started
            );

            let fresh_lease = wait_for_next_lease(pair, &run_id).await;
            pair.owner_a.suspend_and_wait();
            let stopped = snapshot(pair, &run_id).await;
            let stopped_lease = stopped
                .run_lease
                .as_deref()
                .expect("stopped owner lease")
                .parse::<chrono::DateTime<chrono::Utc>>()
                .expect("parse stopped owner lease");
            assert!(stopped_lease >= fresh_lease);
            assert_ready(pair).await;
            let cancellation = abort_run(pair, &run_id).await;
            assert_cancel_ack(&cancellation, pair, &run_id);
            let cancelled = snapshot(pair, &run_id).await;
            assert_eq!(cancelled.run_lease, stopped.run_lease);
            assert_cancel_pending(&cancelled, &baseline, pair, "waiting_for_input");
            assert_sigkill(&mut pair.owner_a);
            assert_ready(pair).await;

            verify_final_fences(pair, PRE_PICKUP_KEY, &run_id, &baseline, &started).await;
        })
    })
    .await;
}

#[tokio::test]
async fn owner_death_after_cancellation_pickup_but_before_terminal_commit_reconciles_once() {
    with_process_pair("ic005-post", true, FIXTURE, |pair| {
        Box::pin(async move {
            let started = start_keyed_run(pair, POST_PICKUP_KEY).await;
            let run_id = started["run_id"].as_str().expect("run id").to_string();
            assert_eq!(started["owner_instance_id"], pair.owner_a_id);
            wait_for_shared_question(pair, &run_id, FIRST_PROMPT).await;
            let baseline = stable_waiting_snapshot(pair, &run_id).await;
            let _fresh_lease = wait_for_next_lease(pair, &run_id).await;
            assert_ready(pair).await;

            let quota_lock = AdvisoryLock::quota(pair).await;
            let cancellation = abort_run(pair, &run_id).await;
            assert_cancel_ack(&cancellation, pair, &run_id);
            wait_for_log(&pair.owner_a, PICKUP_LOG).await;
            wait_until_blocked(pair, quota_lock.backend_pid).await;
            // Dropping the parked Lua future removes its final waiter and
            // restores the still-in-flight row to Running before the monitor's
            // terminal write blocks behind the injected quota lock.
            assert_cancel_pending(&snapshot(pair, &run_id).await, &baseline, pair, "running");
            assert_sigkill(&mut pair.owner_a);
            quota_lock.release().await;

            wait_ready(pair).await;
            verify_final_fences(pair, POST_PICKUP_KEY, &run_id, &baseline, &started).await;
        })
    })
    .await;
}

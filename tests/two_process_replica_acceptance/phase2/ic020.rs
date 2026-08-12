use super::*;

mod fence;
mod http;
mod observability;
mod process;
mod sql;
mod store;

const FIXTURE: &str = include_str!("../../fixtures/two_process_replica/ic020_crew.lua");
const PROMPT: &str = "Hold the IC-020 owner until drain completes";
const FIRST_KEY: &str = "ic020-drain-owner-key-0001";
const REPLACEMENT_KEY: &str = "ic020-replacement-key-0003";
const ENV: &[(&str, &str)] = &[
    ("IRONCREW_HITL_POLL_INTERVAL_MS", "100"),
    ("IRONCREW_EVENT_JOURNAL_POLL_INTERVAL_MS", "100"),
    ("IRONCREW_SHUTDOWN_ROUTING_GRACE_SECS", "1"),
    ("IRONCREW_ADMISSION_WORK_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_WORK_BURST", "100"),
    ("IRONCREW_ADMISSION_CONTROL_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_CONTROL_BURST", "100"),
    ("IRONCREW_ADMISSION_OBSERVATION_RATE_PER_MINUTE", "60000"),
    ("IRONCREW_ADMISSION_OBSERVATION_BURST", "100"),
];

async fn wait_for_initial_snapshot(
    pair: &ProcessPair,
    expected_runs: i64,
    expected_events: i64,
) -> sql::DurableSnapshot {
    sql::wait_for_snapshot(
        pair,
        "one pending question and its journal event",
        |snapshot| {
            snapshot.runs == expected_runs
                && snapshot.idempotency == expected_runs
                && snapshot.pending_questions == 1
                && snapshot.answered_questions == 0
                && snapshot.cancellation_requests == 0
                && snapshot.events == expected_events
        },
    )
    .await
}

async fn scenario(pair: &mut ProcessPair) {
    let peer_id = http::assert_accepting_topology(pair).await;
    sql::assert_owner_index(pair).await;
    let first_owner = pair.owner_a_id.clone();
    let started = http::start_keyed(pair, &pair.owner_a.base_url, &first_owner, FIRST_KEY).await;
    let run_id = started["run_id"]
        .as_str()
        .expect("IC-020 first run id")
        .to_owned();
    let question = wait_for_shared_question(pair, &run_id, PROMPT).await;
    let question_id = question["question_id"]
        .as_str()
        .expect("IC-020 first question id")
        .to_owned();
    let before_drain = wait_for_initial_snapshot(pair, 1, 1).await;
    sql::assert_initial(before_drain);

    process::begin_explicit_drain(&mut pair.owner_a);
    http::wait_draining(pair, &peer_id).await;
    sql::assert_owner_draining(pair, &run_id).await;
    assert_eq!(sql::snapshot(pair).await, before_drain);

    http::assert_mutations_rejected(pair, &peer_id, &run_id, &question_id).await;
    observability::assert_reads_observable(pair, &run_id, &question_id).await;
    assert_eq!(
        sql::snapshot(pair).await,
        before_drain,
        "draining mutations changed durable state"
    );

    // The preceding read-only drain assertions may span most of a heartbeat
    // interval on a contended runner. Align the clean-shutdown boundary with
    // a fresh database-backed lease while retaining the supported six-second
    // minimum that this acceptance test intentionally exercises.
    let _fresh_lease = wait_for_next_lease(pair, &run_id).await;
    process::shutdown_cleanly(
        &mut pair.owner_a,
        &pair.client,
        &pair.survivor_b.base_url,
        "explicitly drained owner A",
    )
    .await;
    assert_eq!(
        wait_for_status_with_context(
            pair,
            &run_id,
            "Aborted",
            "IC-020 explicitly drained owner A",
        )
        .await["status"],
        "Aborted"
    );
    observability::assert_terminal_observable(pair, &peer_id, &run_id).await;
    let after_first_terminal = sql::wait_for_snapshot(
        pair,
        "one aborted terminal event with an empty mailbox",
        |snapshot| snapshot.pending_questions == 0 && snapshot.events == before_drain.events + 1,
    )
    .await;
    sql::assert_one_terminal_event(before_drain, after_first_terminal);
    sql::assert_terminal_fences(pair, &run_id, &first_owner).await;
    observability::assert_replay(pair, &peer_id, FIRST_KEY, &run_id, &first_owner).await;
    assert_eq!(sql::snapshot(pair).await, after_first_terminal);

    let replacement = process::replace_owner(pair, ENV).await;
    let observed_peer = http::assert_accepting_topology(pair).await;
    assert_eq!(observed_peer, peer_id);
    assert_ne!(replacement, first_owner);
    let replacement_started =
        http::start_keyed(pair, &pair.owner_a.base_url, &replacement, REPLACEMENT_KEY).await;
    let replacement_run = replacement_started["run_id"]
        .as_str()
        .expect("IC-020 replacement run id")
        .to_owned();
    wait_for_shared_question(pair, &replacement_run, PROMPT).await;
    let before_direct_sigterm =
        wait_for_initial_snapshot(pair, 2, after_first_terminal.events + 1).await;
    assert_eq!(before_direct_sigterm.runs, after_first_terminal.runs + 1);
    assert_eq!(
        before_direct_sigterm.events,
        after_first_terminal.events + 1
    );

    // The blocked-fence scenario deliberately holds the global run fence for
    // at least one maintenance watchdog. Start it immediately after an
    // observed renewal so CI scheduling delay cannot consume the six-second
    // minimum test lease before the shutdown path gets to prove its retry.
    let _fresh_lease = wait_for_next_lease(pair, &replacement_run).await;
    let owner_fence = fence::RunFenceLock::acquire(pair).await;
    let blocker_pid = owner_fence.backend_pid;
    let sigterm_sent_at = process::signal_terminate(&mut pair.owner_a);
    http::wait_fencing(pair, &peer_id).await;
    wait_until_blocked(pair, blocker_pid).await;
    http::assert_fencing_work_rejected(pair).await;
    assert_eq!(sql::snapshot(pair).await, before_direct_sigterm);
    wait_for_log(
        &pair.owner_a,
        "Owner-drain fence failed; lifecycle remains fencing and shutdown will retry",
    )
    .await;
    process::assert_alive(
        &mut pair.owner_a,
        "replacement C while owner fence is blocked",
    );
    sql::assert_owner_not_draining(pair, &replacement_run).await;
    assert_eq!(sql::snapshot(pair).await, before_direct_sigterm);
    owner_fence.release().await;
    // Direct SIGTERM starts the routing-grace clock before the deliberately
    // blocked fence can commit. Once the lock is released, the listener may
    // therefore close before another HTTP sample can observe `draining`.
    // The earlier `fencing` response proves the monotonic lifecycle transition;
    // use the durable fence for this post-release boundary instead.
    sql::wait_owner_draining(pair, &replacement_run).await;
    process::wait_clean_exit_independently(
        &mut pair.owner_a,
        sigterm_sent_at,
        "replacement C direct SIGTERM",
    )
    .await;
    http::wait_peer_ready(pair, "replacement C direct SIGTERM").await;
    assert_eq!(
        wait_for_status_with_context(
            pair,
            &replacement_run,
            "Aborted",
            "IC-020 replacement C direct SIGTERM",
        )
        .await["status"],
        "Aborted"
    );
    observability::assert_terminal_observable(pair, &peer_id, &replacement_run).await;
    let after_replacement = sql::wait_for_snapshot(
        pair,
        "replacement terminal event with an empty mailbox",
        |snapshot| {
            snapshot.pending_questions == 0 && snapshot.events == before_direct_sigterm.events + 1
        },
    )
    .await;
    sql::assert_one_terminal_event(before_direct_sigterm, after_replacement);
    sql::assert_terminal_fences(pair, &replacement_run, &replacement).await;
}

#[tokio::test]
async fn ic020_explicit_drain_scale_down_and_replacement_are_truthful() {
    with_configured_process_pair("020", true, FIXTURE, ENV, |pair| Box::pin(scenario(pair))).await;
}

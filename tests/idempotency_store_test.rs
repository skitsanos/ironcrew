use std::sync::Arc;

use ironcrew::engine::idempotency::{
    CONVERSATION_MESSAGE_OPERATION, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyLookup, IdempotencyState, RUN_OPERATION, RunFenceHeartbeat,
};
use ironcrew::engine::run_history::{
    JsonFileStore, RunCompletion, RunIntent, RunRecord, RunStatus, RunTransition,
};
use ironcrew::engine::sessions::ConversationRecord;
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::ChatMessage;
use ironcrew::utils::error::IronCrewError;

const CREATED_AT: &str = "2026-07-19T12:00:00Z";
const LEASE_EXPIRES_AT: &str = "2026-07-19T12:01:00Z";
const COMPLETED_AT: &str = "2026-07-19T12:00:10Z";
const RETENTION_EXPIRES_AT: &str = "2026-07-19T12:10:10Z";

fn digest(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

fn numbered_digest(value: u64) -> String {
    format!("{value:064x}")
}

fn timestamp_after(timestamp: &str, duration: std::time::Duration) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .unwrap()
        .checked_add_signed(chrono::Duration::from_std(duration).unwrap())
        .unwrap()
        .to_rfc3339()
}

fn conversation_claim(
    key_byte: char,
    fingerprint_byte: char,
    attempt: &str,
    resource_id: &str,
    exclusive_scope: &str,
) -> IdempotencyClaim {
    IdempotencyClaim {
        key_hash: digest(key_byte),
        recovery_key_hash: None,
        request_fingerprint: digest(fingerprint_byte),
        operation: CONVERSATION_MESSAGE_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: resource_id.into(),
        exclusive_scope: Some(exclusive_scope.into()),
        attempt_id: attempt.into(),
        owner_instance_id: "pod-a".into(),
        base_revision: Some(0),
        response_status: None,
        response_body: None,
        max_total_response_bytes: usize::MAX,
        lease_expires_at: LEASE_EXPIRES_AT.into(),
        created_at: CREATED_AT.into(),
        ttl_seconds: 600,
    }
}

fn run_claim(
    key: u64,
    owner_instance_id: &str,
    run_id: &str,
    attempt_id: &str,
) -> IdempotencyClaim {
    IdempotencyClaim {
        key_hash: numbered_digest(key),
        recovery_key_hash: None,
        request_fingerprint: numbered_digest(key.saturating_add(100)),
        operation: RUN_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: run_id.into(),
        exclusive_scope: None,
        attempt_id: attempt_id.into(),
        owner_instance_id: owner_instance_id.into(),
        base_revision: None,
        response_status: Some(200),
        response_body: Some(format!("{{\"run_id\":\"{run_id}\"}}")),
        max_total_response_bytes: usize::MAX,
        lease_expires_at: LEASE_EXPIRES_AT.into(),
        created_at: CREATED_AT.into(),
        ttl_seconds: 600,
    }
}

fn completion(claim: &IdempotencyClaim, body: &str) -> IdempotencyCompletion {
    IdempotencyCompletion {
        key_hash: claim.key_hash.clone(),
        request_fingerprint: claim.request_fingerprint.clone(),
        attempt_id: claim.attempt_id.clone(),
        owner_instance_id: claim.owner_instance_id.clone(),
        response_status: 200,
        response_body: Some(body.into()),
        completed_at: COMPLETED_AT.into(),
        expires_at: RETENTION_EXPIRES_AT.into(),
    }
}

fn conversation(id: &str) -> ConversationRecord {
    ConversationRecord {
        id: id.into(),
        flow_name: "Flow A".into(),
        flow_path: Some("flow-a".into()),
        agent_name: "assistant".into(),
        messages: vec![ChatMessage::user("hello")],
        created_at: CREATED_AT.into(),
        updated_at: COMPLETED_AT.into(),
        revision: 0,
    }
}

async fn exercise_idempotency_contract(
    store: Arc<dyn StateStore>,
    competing_store: Arc<dyn StateStore>,
) {
    let claim = conversation_claim(
        'a',
        'b',
        "attempt-a",
        "conversation-a",
        "flow-a:conversation-a",
    );
    let left_store = Arc::clone(&store);
    let right_store = competing_store;
    let left_claim = claim.clone();
    let right_claim = claim.clone();
    let (left, right) = tokio::join!(
        left_store.claim_idempotency(left_claim, 100, 10),
        right_store.claim_idempotency(right_claim, 100, 10)
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, IdempotencyClaimOutcome::Claimed(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, IdempotencyClaimOutcome::InProgress(_)))
            .count(),
        1
    );

    let mut mismatched = claim.clone();
    mismatched.request_fingerprint = digest('c');
    assert!(matches!(
        store.claim_idempotency(mismatched, 100, 10).await.unwrap(),
        IdempotencyClaimOutcome::Conflict
    ));

    let busy = conversation_claim(
        'd',
        'e',
        "attempt-b",
        "conversation-a",
        "flow-a:conversation-a",
    );
    assert!(matches!(
        store.claim_idempotency(busy, 100, 10).await.unwrap(),
        IdempotencyClaimOutcome::Busy
    ));

    assert!(
        store
            .save_conversation(&conversation("conversation-a"))
            .await
            .is_err()
    );
    assert!(
        store
            .delete_conversation(Some("flow-a"), "conversation-a")
            .await
            .is_err()
    );

    let mut stale_completion = completion(&claim, "{\"ok\":true}");
    stale_completion.attempt_id = "stale-attempt".into();
    assert!(matches!(
        store
            .complete_idempotency(stale_completion, 4096)
            .await
            .unwrap_err(),
        IronCrewError::Conflict(_)
    ));

    let committed = store
        .commit_conversation_idempotency(
            completion(&claim, "{\"ok\":true}"),
            &conversation("conversation-a"),
            4096,
        )
        .await
        .unwrap();
    assert_eq!(committed.revision, 1);
    assert!(committed.replayable);
    assert!(!committed.already_completed);
    let stored = store
        .get_conversation(Some("flow-a"), "conversation-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.revision, 1);
    assert_eq!(stored.messages.len(), 1);

    assert!(matches!(
        store
            .lookup_idempotency(&claim.key_hash, &claim.request_fingerprint, COMPLETED_AT)
            .await
            .unwrap(),
        IdempotencyLookup::Replay(_)
    ));
    assert!(
        store
            .heartbeat_idempotency(&claim.key_hash, &claim.attempt_id, "2026-07-19T12:02:00Z")
            .await
            .unwrap(),
        "same-attempt completion is not a lost fence"
    );
    let replay_commit = store
        .commit_conversation_idempotency(
            completion(&claim, "{\"ok\":true}"),
            &conversation("conversation-a"),
            4096,
        )
        .await
        .unwrap();
    assert!(replay_commit.already_completed);
    assert_eq!(replay_commit.revision, 1);

    let stale_revision = conversation_claim(
        '0',
        '1',
        "stale-revision-attempt",
        "conversation-a",
        "flow-a:conversation-a",
    );
    assert!(matches!(
        store
            .claim_idempotency(stale_revision.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Conflict
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &stale_revision.key_hash,
                &stale_revision.request_fingerprint,
                COMPLETED_AT
            )
            .await
            .unwrap(),
        IdempotencyLookup::Miss
    ));

    let mut missing_with_stale_revision = conversation_claim(
        'c',
        'd',
        "missing-stale-attempt",
        "missing-conversation",
        "flow-a:missing-conversation",
    );
    missing_with_stale_revision.base_revision = Some(1);
    assert!(matches!(
        store
            .claim_idempotency(missing_with_stale_revision, 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Conflict
    ));

    let mut expired_hazard = conversation_claim(
        'b',
        'e',
        "expired-hazard-attempt",
        "hazard-conversation",
        "flow-a:hazard-conversation",
    );
    expired_hazard.created_at = "2026-07-19T11:58:00Z".into();
    expired_hazard.lease_expires_at = "2026-07-19T11:59:00Z".into();
    assert!(matches!(
        store
            .claim_idempotency(expired_hazard.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));

    let discovery = conversation_claim(
        '3',
        '4',
        "hazard-discovery-attempt",
        "hazard-conversation",
        "flow-a:hazard-conversation",
    );
    assert!(matches!(
        store
            .claim_idempotency(discovery.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &discovery.key_hash,
                &discovery.request_fingerprint,
                CREATED_AT
            )
            .await
            .unwrap(),
        IdempotencyLookup::Miss
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &expired_hazard.key_hash,
                &expired_hazard.request_fingerprint,
                CREATED_AT
            )
            .await
            .unwrap(),
        IdempotencyLookup::Indeterminate(ref record)
            if record.exclusive_scope.as_deref() == Some("flow-a:hazard-conversation")
    ));

    let without_recovery = conversation_claim(
        '5',
        '6',
        "without-recovery-attempt",
        "hazard-conversation",
        "flow-a:hazard-conversation",
    );
    assert!(matches!(
        store
            .claim_idempotency(without_recovery, 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));

    let mut mismatched_recovery = conversation_claim(
        '7',
        '8',
        "mismatched-recovery-attempt",
        "hazard-conversation",
        "flow-a:hazard-conversation",
    );
    mismatched_recovery.recovery_key_hash = Some(digest('a'));
    assert!(matches!(
        store
            .claim_idempotency(mismatched_recovery, 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));

    let mut matching_recovery = conversation_claim(
        '9',
        '0',
        "matching-recovery-attempt",
        "hazard-conversation",
        "flow-a:hazard-conversation",
    );
    matching_recovery.recovery_key_hash = Some(expired_hazard.key_hash.clone());
    assert!(matches!(
        store
            .claim_idempotency(matching_recovery.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Busy
    ));

    let recovery_grace_ttl = store.run_lease_ttl();
    matching_recovery.created_at = timestamp_after(CREATED_AT, recovery_grace_ttl);
    matching_recovery.lease_expires_at =
        timestamp_after(&matching_recovery.created_at, recovery_grace_ttl);
    assert!(matches!(
        store
            .claim_idempotency(matching_recovery.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &matching_recovery.key_hash,
                &matching_recovery.request_fingerprint,
                &matching_recovery.created_at
            )
            .await
            .unwrap(),
        IdempotencyLookup::InProgress(_)
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &expired_hazard.key_hash,
                &expired_hazard.request_fingerprint,
                CREATED_AT
            )
            .await
            .unwrap(),
        IdempotencyLookup::Indeterminate(ref record) if record.exclusive_scope.is_none()
    ));

    let expired = IdempotencyClaim {
        key_hash: digest('f'),
        recovery_key_hash: None,
        request_fingerprint: digest('1'),
        operation: CONVERSATION_MESSAGE_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: "expired-conversation".into(),
        exclusive_scope: Some("flow-a:expired-conversation".into()),
        attempt_id: "attempt-expired".into(),
        owner_instance_id: "pod-a".into(),
        base_revision: Some(0),
        response_status: None,
        response_body: None,
        max_total_response_bytes: usize::MAX,
        lease_expires_at: "2026-07-19T11:59:00Z".into(),
        created_at: "2026-07-19T11:58:00Z".into(),
        ttl_seconds: 60,
    };
    assert!(matches!(
        store
            .claim_idempotency(expired.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    let mut retry = expired.clone();
    retry.created_at = CREATED_AT.into();
    retry.lease_expires_at = LEASE_EXPIRES_AT.into();
    retry.attempt_id = "attempt-retry".into();
    assert!(matches!(
        store.claim_idempotency(retry, 100, 10).await.unwrap(),
        IdempotencyClaimOutcome::Indeterminate(_)
    ));
    assert!(matches!(
        store
            .lookup_idempotency(&expired.key_hash, &expired.request_fingerprint, CREATED_AT)
            .await
            .unwrap(),
        IdempotencyLookup::Indeterminate(_)
    ));
    assert_eq!(
        store
            .prune_idempotency("2026-07-19T12:02:00Z", 1)
            .await
            .unwrap(),
        1
    );
    assert!(matches!(
        store
            .lookup_idempotency(
                &expired.key_hash,
                &expired.request_fingerprint,
                "2026-07-19T12:02:00Z"
            )
            .await
            .unwrap(),
        IdempotencyLookup::Miss
    ));

    let run_claim = IdempotencyClaim {
        key_hash: digest('2'),
        recovery_key_hash: None,
        request_fingerprint: digest('3'),
        operation: RUN_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: "run-a".into(),
        exclusive_scope: None,
        attempt_id: "run-attempt".into(),
        owner_instance_id: "pod-a".into(),
        base_revision: None,
        response_status: Some(202),
        response_body: Some("{\"run_id\":\"run-a\"}".into()),
        max_total_response_bytes: usize::MAX,
        lease_expires_at: LEASE_EXPIRES_AT.into(),
        created_at: CREATED_AT.into(),
        ttl_seconds: 600,
    };
    assert!(matches!(
        store
            .claim_idempotency(run_claim.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        store.claim_idempotency(run_claim, 100, 10).await.unwrap(),
        IdempotencyClaimOutcome::InProgress(_)
    ));
}

async fn exercise_claim_response_budget(store: Arc<dyn StateStore>) {
    let owner = store.instance_id().to_string();
    let first_body = "{\"run_id\":\"budget-first\"}".to_string();
    let aggregate_budget = first_body.len();

    let mut first = run_claim(20, &owner, "budget-first", "budget-first-attempt");
    first.response_body = Some(first_body.clone());
    first.max_total_response_bytes = aggregate_budget;
    let first_record = match store
        .claim_idempotency(first.clone(), 100, 10)
        .await
        .unwrap()
    {
        IdempotencyClaimOutcome::Claimed(record) => record,
        outcome => panic!("unexpected first budget claim outcome: {outcome:?}"),
    };
    assert_eq!(
        first_record.response_body.as_deref(),
        Some(first_body.as_str())
    );
    store
        .save_run_intent(RunIntent {
            suggested_id: Some(first.resource_id.clone()),
            flow_name: "First budget run".into(),
            flow: first.scope.clone(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await
        .unwrap();

    let mut second = run_claim(21, &owner, "budget-second", "budget-second-attempt");
    second.response_body = Some("{\"run_id\":\"budget-second\"}".into());
    second.max_total_response_bytes = aggregate_budget;
    let second_record = match store
        .claim_idempotency(second.clone(), 100, 10)
        .await
        .unwrap()
    {
        IdempotencyClaimOutcome::Claimed(record) => record,
        outcome => panic!("unexpected second budget claim outcome: {outcome:?}"),
    };
    assert_eq!(second_record.response_status, Some(200));
    assert!(second_record.response_body.is_none());
    assert!(!second_record.replayable());
    store
        .save_run_intent(RunIntent {
            suggested_id: Some(second.resource_id.clone()),
            flow_name: "Second budget run".into(),
            flow: second.scope.clone(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await
        .unwrap();

    assert_eq!(
        store
            .reconcile_abandoned_runs("2099-07-19T12:00:00Z")
            .await
            .unwrap(),
        2
    );
    let retained = match store
        .lookup_idempotency(
            &first.key_hash,
            &first.request_fingerprint,
            "2099-07-19T12:00:01Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Replay(record) => record,
        outcome => panic!("unexpected retained budget lookup: {outcome:?}"),
    };
    let tombstone = match store
        .lookup_idempotency(
            &second.key_hash,
            &second.request_fingerprint,
            "2099-07-19T12:00:01Z",
        )
        .await
        .unwrap()
    {
        IdempotencyLookup::Indeterminate(record) => record,
        outcome => panic!("unexpected tombstoned budget lookup: {outcome:?}"),
    };
    assert_eq!(tombstone.state, IdempotencyState::Completed);
    assert_eq!(tombstone.response_status, Some(200));
    assert!(tombstone.response_body.is_none());
    assert!(!tombstone.replayable());
    let aggregate_response_bytes = retained
        .response_body
        .as_deref()
        .map(str::len)
        .unwrap_or(0)
        .checked_add(
            tombstone
                .response_body
                .as_deref()
                .map(str::len)
                .unwrap_or(0),
        )
        .unwrap();
    assert_eq!(aggregate_response_bytes, aggregate_budget);
}

async fn exercise_run_lifecycle_reconciliation(store: Arc<dyn StateStore>) {
    let owner = store.instance_id().to_string();
    let mapped_claim = IdempotencyClaim {
        key_hash: digest('4'),
        recovery_key_hash: None,
        request_fingerprint: digest('5'),
        operation: RUN_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: "mapped-run".into(),
        exclusive_scope: None,
        attempt_id: "mapped-attempt".into(),
        owner_instance_id: owner.clone(),
        base_revision: None,
        response_status: Some(200),
        response_body: Some("{\"run_id\":\"mapped-run\"}".into()),
        max_total_response_bytes: usize::MAX,
        lease_expires_at: LEASE_EXPIRES_AT.into(),
        created_at: CREATED_AT.into(),
        ttl_seconds: 600,
    };
    assert!(matches!(
        store
            .claim_idempotency(mapped_claim.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    store
        .save_run_intent(RunIntent {
            suggested_id: Some("mapped-run".into()),
            flow_name: "Mapped run".into(),
            flow: "flow-a".into(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await
        .unwrap();
    let running = store
        .lookup_idempotency(
            &mapped_claim.key_hash,
            &mapped_claim.request_fingerprint,
            CREATED_AT,
        )
        .await
        .unwrap();
    assert!(matches!(
        running,
        IdempotencyLookup::Replay(ref record) if record.state == IdempotencyState::Running
    ));

    let transition = store
        .update_run_completion(
            "mapped-run",
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "2026-07-19T12:00:30Z".into(),
                duration_ms: 30_000,
                task_results: Vec::new(),
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(transition, RunTransition::Applied);
    let completed = store
        .lookup_idempotency(
            &mapped_claim.key_hash,
            &mapped_claim.request_fingerprint,
            "2026-07-19T12:00:31Z",
        )
        .await
        .unwrap();
    assert!(matches!(
        completed,
        IdempotencyLookup::Replay(ref record)
            if record.state == IdempotencyState::Completed
                && record.completed_at.as_deref() == Some("2026-07-19T12:00:30Z")
    ));

    let orphaned_claim = IdempotencyClaim {
        key_hash: digest('6'),
        recovery_key_hash: None,
        request_fingerprint: digest('7'),
        operation: RUN_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: "orphaned-run".into(),
        exclusive_scope: None,
        attempt_id: "orphaned-attempt".into(),
        owner_instance_id: owner.clone(),
        base_revision: None,
        response_status: Some(200),
        response_body: Some("{\"run_id\":\"orphaned-run\"}".into()),
        max_total_response_bytes: usize::MAX,
        lease_expires_at: "2026-07-19T11:01:00Z".into(),
        created_at: "2026-07-19T11:00:00Z".into(),
        ttl_seconds: 600,
    };
    store
        .claim_idempotency(orphaned_claim.clone(), 100, 10)
        .await
        .unwrap();

    let expired_message = IdempotencyClaim {
        key_hash: digest('8'),
        recovery_key_hash: None,
        request_fingerprint: digest('9'),
        operation: CONVERSATION_MESSAGE_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: "reconciled-conversation".into(),
        exclusive_scope: Some("flow-a:reconciled-conversation".into()),
        attempt_id: "expired-message-attempt".into(),
        owner_instance_id: owner,
        base_revision: Some(0),
        response_status: None,
        response_body: None,
        max_total_response_bytes: usize::MAX,
        lease_expires_at: "2026-07-19T11:01:00Z".into(),
        created_at: "2026-07-19T11:00:00Z".into(),
        ttl_seconds: 600,
    };
    store
        .claim_idempotency(expired_message.clone(), 100, 10)
        .await
        .unwrap();

    assert_eq!(
        store
            .reconcile_abandoned_runs("2026-07-19T12:00:00Z")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.get_run("orphaned-run").await.unwrap().status,
        RunStatus::Abandoned
    );
    assert!(matches!(
        store
            .lookup_idempotency(
                &orphaned_claim.key_hash,
                &orphaned_claim.request_fingerprint,
                "2026-07-19T12:00:01Z"
            )
            .await
            .unwrap(),
        IdempotencyLookup::Replay(ref record) if record.state == IdempotencyState::Completed
    ));
    assert!(matches!(
        store
            .lookup_idempotency(
                &expired_message.key_hash,
                &expired_message.request_fingerprint,
                "2026-07-19T12:00:01Z"
            )
            .await
            .unwrap(),
        IdempotencyLookup::Indeterminate(_)
    ));

    let late_completion = completion(&expired_message, "{\"late\":true}");
    assert!(matches!(
        store
            .complete_idempotency(late_completion.clone(), 4096)
            .await
            .unwrap_err(),
        IronCrewError::Conflict(_)
    ));
    assert!(matches!(
        store
            .commit_conversation_idempotency(
                late_completion,
                &conversation("reconciled-conversation"),
                4096,
            )
            .await
            .unwrap_err(),
        IronCrewError::Conflict(_)
    ));
    assert!(
        store
            .get_conversation(Some("flow-a"), "reconciled-conversation")
            .await
            .unwrap()
            .is_none()
    );

    let conversation = conversation("reconciled-conversation");
    assert_eq!(store.save_conversation(&conversation).await.unwrap(), 1);
    store
        .delete_conversation(Some("flow-a"), "reconciled-conversation")
        .await
        .unwrap();
}

async fn exercise_idempotent_run_heartbeat(store: Arc<dyn StateStore>) {
    const RENEWED_LEASE: &str = "2099-07-19T12:05:00Z";
    let owner = store.instance_id().to_string();

    assert_eq!(
        store
            .heartbeat_idempotent_run(
                "missing-fence-run",
                &numbered_digest(1),
                "missing-attempt",
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );

    let claimed_only = run_claim(2, &owner, "claimed-only-run", "claimed-only-attempt");
    assert!(matches!(
        store
            .claim_idempotency(claimed_only.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        store
            .heartbeat_idempotent_run(
                &claimed_only.resource_id,
                &claimed_only.key_hash,
                "stale-attempt",
                RENEWED_LEASE,
            )
            .await
            .unwrap_err(),
        IronCrewError::Conflict(_)
    ));
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                "different-run",
                &claimed_only.key_hash,
                &claimed_only.attempt_id,
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &claimed_only.resource_id,
                &claimed_only.key_hash,
                &claimed_only.attempt_id,
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Owned
    );
    assert!(matches!(
        store
            .lookup_idempotency(
                &claimed_only.key_hash,
                &claimed_only.request_fingerprint,
                CREATED_AT,
            )
            .await
            .unwrap(),
        IdempotencyLookup::InProgress(ref record)
            if record.state == IdempotencyState::Claimed
                && record.lease_expires_at == RENEWED_LEASE
    ));
    store
        .complete_idempotency(completion(&claimed_only, "{\"accepted\":true}"), 4096)
        .await
        .unwrap();
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &claimed_only.resource_id,
                &claimed_only.key_hash,
                &claimed_only.attempt_id,
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost,
        "a completed ledger without a terminal run is not an ownership fence"
    );

    let indeterminate = run_claim(3, &owner, "indeterminate-run", "indeterminate-attempt");
    store
        .claim_idempotency(indeterminate.clone(), 100, 10)
        .await
        .unwrap();
    assert!(
        store
            .mark_idempotency_indeterminate(
                &indeterminate.key_hash,
                &indeterminate.attempt_id,
                COMPLETED_AT,
                RETENTION_EXPIRES_AT,
            )
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &indeterminate.resource_id,
                &indeterminate.key_hash,
                &indeterminate.attempt_id,
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );

    let foreign_ledger = run_claim(4, "foreign-owner", "foreign-ledger-run", "foreign-attempt");
    store
        .claim_idempotency(foreign_ledger.clone(), 100, 10)
        .await
        .unwrap();
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &foreign_ledger.resource_id,
                &foreign_ledger.key_hash,
                &foreign_ledger.attempt_id,
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );

    let running = run_claim(5, &owner, "fenced-running-run", "running-attempt");
    store
        .claim_idempotency(running.clone(), 100, 10)
        .await
        .unwrap();
    store
        .save_run_intent(RunIntent {
            suggested_id: Some(running.resource_id.clone()),
            flow_name: "Fenced running run".into(),
            flow: running.scope.clone(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &running.resource_id,
                &running.key_hash,
                &running.attempt_id,
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Owned
    );
    assert_eq!(
        store
            .get_run(&running.resource_id)
            .await
            .unwrap()
            .lease_expires_at,
        RENEWED_LEASE
    );
    assert!(matches!(
        store
            .lookup_idempotency(&running.key_hash, &running.request_fingerprint, CREATED_AT)
            .await
            .unwrap(),
        IdempotencyLookup::Replay(ref record)
            if record.state == IdempotencyState::Running
                && record.lease_expires_at == RENEWED_LEASE
    ));

    let non_keyed_id = store
        .save_run_intent(RunIntent {
            suggested_id: Some("non-keyed-heartbeat-run".into()),
            flow_name: "Non-keyed heartbeat run".into(),
            flow: "flow-a".into(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await
        .unwrap();
    let non_keyed_before = store.get_run(&non_keyed_id).await.unwrap().lease_expires_at;
    assert_eq!(store.heartbeat_owned_runs().await.unwrap(), 1);
    assert_eq!(
        store
            .get_run(&running.resource_id)
            .await
            .unwrap()
            .lease_expires_at,
        RENEWED_LEASE,
        "global heartbeats must skip keyed runs"
    );
    assert!(matches!(
        store
            .lookup_idempotency(&running.key_hash, &running.request_fingerprint, CREATED_AT)
            .await
            .unwrap(),
        IdempotencyLookup::Replay(ref record) if record.lease_expires_at == RENEWED_LEASE
    ));
    let non_keyed_after = store.get_run(&non_keyed_id).await.unwrap().lease_expires_at;
    assert!(
        chrono::DateTime::parse_from_rfc3339(&non_keyed_after).unwrap()
            >= chrono::DateTime::parse_from_rfc3339(&non_keyed_before).unwrap()
    );

    store
        .update_run_completion(
            &running.resource_id,
            RunCompletion {
                status: RunStatus::Success,
                finished_at: "2099-07-19T12:05:10Z".into(),
                duration_ms: 10,
                task_results: Vec::new(),
                total_tokens: 0,
                cached_tokens: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &running.resource_id,
                &running.key_hash,
                &running.attempt_id,
                "2099-07-19T12:06:00Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Terminal(RunStatus::Success)
    );

    let missing_run = run_claim(6, &owner, "missing-running-run", "missing-running-attempt");
    store
        .claim_idempotency(missing_run.clone(), 100, 10)
        .await
        .unwrap();
    store
        .save_run_intent(RunIntent {
            suggested_id: Some(missing_run.resource_id.clone()),
            flow_name: "Missing running run".into(),
            flow: missing_run.scope.clone(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await
        .unwrap();
    store.delete_run(&missing_run.resource_id).await.unwrap();
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &missing_run.resource_id,
                &missing_run.key_hash,
                &missing_run.attempt_id,
                RENEWED_LEASE,
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );
}

async fn create_wrong_owner_run_fixture(store: Arc<dyn StateStore>) -> IdempotencyClaim {
    let claim = run_claim(
        7,
        store.instance_id(),
        "wrong-owner-fenced-run",
        "wrong-owner-attempt",
    );
    store
        .claim_idempotency(claim.clone(), 100, 10)
        .await
        .unwrap();
    store
        .save_run_intent(RunIntent {
            suggested_id: Some(claim.resource_id.clone()),
            flow_name: "Wrong-owner fenced run".into(),
            flow: claim.scope.clone(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await
        .unwrap();
    claim
}

async fn assert_wrong_owner_run_loses_fence(store: Arc<dyn StateStore>, claim: &IdempotencyClaim) {
    assert_eq!(
        store
            .heartbeat_idempotent_run(
                &claim.resource_id,
                &claim.key_hash,
                &claim.attempt_id,
                "2099-07-19T12:05:00Z",
            )
            .await
            .unwrap(),
        RunFenceHeartbeat::Lost
    );
}

async fn exercise_provisional_run_hydration(store: Arc<dyn StateStore>) {
    let owner = store.instance_id().to_string();
    let claim = IdempotencyClaim {
        key_hash: digest('d'),
        recovery_key_hash: None,
        request_fingerprint: digest('c'),
        operation: RUN_OPERATION.into(),
        scope: "flow-a".into(),
        resource_id: "provisional-run".into(),
        exclusive_scope: None,
        attempt_id: "provisional-attempt".into(),
        owner_instance_id: owner,
        base_revision: None,
        response_status: Some(200),
        response_body: Some("{\"run_id\":\"provisional-run\"}".into()),
        max_total_response_bytes: usize::MAX,
        lease_expires_at: LEASE_EXPIRES_AT.into(),
        created_at: CREATED_AT.into(),
        ttl_seconds: 600,
    };
    assert!(matches!(
        store
            .claim_idempotency(claim.clone(), 100, 10)
            .await
            .unwrap(),
        IdempotencyClaimOutcome::Claimed(_)
    ));
    store
        .save_run_intent(RunIntent {
            suggested_id: Some("provisional-run".into()),
            flow_name: "flow-a".into(),
            flow: "flow-a".into(),
            started_at: CREATED_AT.into(),
            agent_count: 0,
            task_count: 0,
            tags: vec!["provisional".into()],
        })
        .await
        .unwrap();
    let provisional = store.get_run("provisional-run").await.unwrap();
    store
        .update_run_status("provisional-run", RunStatus::WaitingForInput)
        .await
        .unwrap();

    store
        .save_run_intent(RunIntent {
            suggested_id: Some("provisional-run".into()),
            flow_name: "Hydrated crew goal".into(),
            flow: "flow-a".into(),
            started_at: "2026-07-19T13:00:00Z".into(),
            agent_count: 2,
            task_count: 3,
            tags: vec!["hydrated".into()],
        })
        .await
        .unwrap();
    let hydrated = store.get_run("provisional-run").await.unwrap();
    assert_eq!(hydrated.status, RunStatus::WaitingForInput);
    assert_eq!(hydrated.flow_name, "Hydrated crew goal");
    assert_eq!(hydrated.agent_count, 2);
    assert_eq!(hydrated.task_count, 3);
    assert_eq!(hydrated.tags, vec!["hydrated"]);
    assert_eq!(hydrated.started_at, CREATED_AT);
    assert!(
        chrono::DateTime::parse_from_rfc3339(&hydrated.lease_expires_at).unwrap()
            >= chrono::DateTime::parse_from_rfc3339(&provisional.lease_expires_at).unwrap()
    );
    assert!(matches!(
        store
            .lookup_idempotency(&claim.key_hash, &claim.request_fingerprint, CREATED_AT)
            .await
            .unwrap(),
        IdempotencyLookup::Replay(ref record)
            if record.state == IdempotencyState::Running
                && record.lease_expires_at == hydrated.lease_expires_at
    ));

    store
        .complete_idempotency(completion(&claim, "{\"run_id\":\"provisional-run\"}"), 4096)
        .await
        .unwrap();
    store
        .save_run_intent(RunIntent {
            suggested_id: Some("provisional-run".into()),
            flow_name: "Hydrated after acceptance".into(),
            flow: "flow-a".into(),
            started_at: "2026-07-19T14:00:00Z".into(),
            agent_count: 4,
            task_count: 5,
            tags: vec!["completed-ledger".into()],
        })
        .await
        .unwrap();
    let completed_ledger_hydration = store.get_run("provisional-run").await.unwrap();
    assert_eq!(
        completed_ledger_hydration.flow_name,
        "Hydrated after acceptance"
    );
    assert_eq!(completed_ledger_hydration.agent_count, 4);
    assert_eq!(completed_ledger_hydration.task_count, 5);
    assert_eq!(completed_ledger_hydration.started_at, CREATED_AT);

    let unlinked = RunIntent {
        suggested_id: Some("unlinked-duplicate".into()),
        flow_name: "Unlinked".into(),
        flow: "flow-a".into(),
        started_at: CREATED_AT.into(),
        agent_count: 1,
        task_count: 1,
        tags: Vec::new(),
    };
    store.save_run_intent(unlinked.clone()).await.unwrap();
    assert!(store.save_run_intent(unlinked).await.is_err());
}

#[tokio::test]
async fn json_store_enforces_idempotency_contract() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join(".ironcrew");
    let store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(root.clone()).expect("create JSON store"));
    let competing_store: Arc<dyn StateStore> =
        Arc::new(JsonFileStore::new(root.clone()).expect("create second JSON store"));
    exercise_claim_response_budget(Arc::clone(&store)).await;
    exercise_idempotency_contract(Arc::clone(&store), competing_store).await;
    exercise_run_lifecycle_reconciliation(Arc::clone(&store)).await;
    exercise_idempotent_run_heartbeat(Arc::clone(&store)).await;
    let wrong_owner = create_wrong_owner_run_fixture(Arc::clone(&store)).await;
    let run_path = root
        .join("runs")
        .join(format!("{}.json", wrong_owner.resource_id));
    let mut wrong_owner_run: RunRecord =
        serde_json::from_slice(&std::fs::read(&run_path).unwrap()).unwrap();
    wrong_owner_run.owner_instance_id = "foreign-owner".into();
    std::fs::write(
        &run_path,
        serde_json::to_vec_pretty(&wrong_owner_run).unwrap(),
    )
    .unwrap();
    assert_wrong_owner_run_loses_fence(Arc::clone(&store), &wrong_owner).await;
    exercise_provisional_run_hydration(Arc::clone(&store)).await;

    std::fs::write(
        root.join("idempotency")
            .join(format!("{}.json", digest('e'))),
        b"{corrupt",
    )
    .unwrap();
    let rollback_result = store
        .save_run_intent(RunIntent {
            suggested_id: Some("rollback-run".into()),
            flow_name: "Rollback".into(),
            flow: "flow-a".into(),
            started_at: CREATED_AT.into(),
            agent_count: 1,
            task_count: 1,
            tags: Vec::new(),
        })
        .await;
    assert!(rollback_result.is_err());
    assert!(!root.join("runs").join("rollback-run.json").exists());
}

#[tokio::test]
async fn sqlite_store_enforces_idempotency_contract() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let store: Arc<dyn StateStore> =
        Arc::new(SqliteStore::new(path.clone()).expect("create SQLite store"));
    let competing_store: Arc<dyn StateStore> =
        Arc::new(SqliteStore::new(path.clone()).expect("create second SQLite store"));
    exercise_claim_response_budget(Arc::clone(&store)).await;
    exercise_idempotency_contract(Arc::clone(&store), competing_store).await;
    exercise_run_lifecycle_reconciliation(Arc::clone(&store)).await;
    exercise_idempotent_run_heartbeat(Arc::clone(&store)).await;
    let wrong_owner = create_wrong_owner_run_fixture(Arc::clone(&store)).await;
    let conn = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        conn.execute(
            "UPDATE runs SET owner_instance_id = 'foreign-owner' WHERE run_id = ?1",
            rusqlite::params![&wrong_owner.resource_id],
        )
        .unwrap(),
        1
    );
    drop(conn);
    assert_wrong_owner_run_loses_fence(Arc::clone(&store), &wrong_owner).await;
    exercise_provisional_run_hydration(store).await;
}

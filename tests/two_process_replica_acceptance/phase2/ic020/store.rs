use std::panic::{AssertUnwindSafe, resume_unwind};

use futures::FutureExt;
use ironcrew::engine::human_input::{
    DurableHumanInputRegistration, HumanInputAnswerOutcome, HumanInputKeyring,
    HumanInputListOutcome, HumanInputRegistrationOutcome,
};
use ironcrew::engine::idempotency::{
    IdempotencyClaim, IdempotencyClaimOutcome, IdempotencyLimits, PrincipalId, RUN_OPERATION,
};
use ironcrew::engine::input_bridge::QuestionInfo;
use ironcrew::engine::run_history::RunIntent;
use ironcrew::engine::store::StateStore;
use ironcrew::utils::error::IronCrewError;
use sqlx::Row;

use super::super::*;

const FLOW_NAME: &str = "replica-acceptance";
const OWNER_ID: &str = "ic020-store-owner";

fn run_intent(run_id: &str) -> RunIntent {
    RunIntent {
        suggested_id: Some(run_id.into()),
        flow_name: "IC-020 store fence".into(),
        flow: FLOW_NAME.into(),
        started_at: "2026-08-10T00:00:00Z".into(),
        agent_count: 0,
        task_count: 0,
        tags: Vec::new(),
    }
}

fn claim(run_id: &str, marker: char) -> IdempotencyClaim {
    let fingerprint_marker = match marker {
        'a' => 'c',
        'b' => 'd',
        _ => panic!("unsupported IC-020 claim marker"),
    };
    IdempotencyClaim {
        key_hash: marker.to_string().repeat(64),
        principal_id: PrincipalId::legacy(),
        recovery_key_hash: None,
        request_fingerprint: fingerprint_marker.to_string().repeat(64),
        operation: RUN_OPERATION.into(),
        scope: FLOW_NAME.into(),
        resource_id: run_id.into(),
        exclusive_scope: None,
        attempt_id: format!("ic020-attempt-{marker}"),
        owner_instance_id: OWNER_ID.into(),
        base_revision: None,
        response_status: Some(200),
        response_body: Some(format!(r#"{{"run_id":"{run_id}"}}"#)),
        max_total_response_bytes: usize::MAX,
        lease_expires_at: "9999-01-01T00:00:00Z".into(),
        created_at: "2026-08-10T00:00:00Z".into(),
        ttl_seconds: 86_400,
    }
}

fn limits() -> IdempotencyLimits {
    IdempotencyLimits {
        global_max_records: 10,
        principal_max_records: 10,
        principal_max_in_flight: 10,
        global_max_response_bytes: 1024,
        principal_max_response_bytes: 1024,
        prune_batch: 10,
    }
}

fn registration(
    run_id: &str,
    question_id: &str,
    claim: &IdempotencyClaim,
) -> DurableHumanInputRegistration {
    DurableHumanInputRegistration {
        flow: FLOW_NAME.into(),
        run_id: run_id.into(),
        question: QuestionInfo {
            question_id: question_id.into(),
            prompt: "Keep this exact run fenced?".into(),
            choices: vec!["yes".into(), "no".into()],
            asked_at: "2026-08-10T00:00:00Z".into(),
            timeout_s: 600,
            kind: "approval".into(),
        },
        key_hash: claim.key_hash.clone(),
        attempt_id: claim.attempt_id.clone(),
    }
}

async fn claim_only(store: &PostgresStore, claim: &IdempotencyClaim) {
    assert!(matches!(
        store
            .claim_idempotency_with_limits(claim.clone(), limits())
            .await
            .expect("claim IC-020 store run"),
        IdempotencyClaimOutcome::Claimed(_)
    ));
}

async fn scenario(database_url: &str, prefix: &str) {
    let keyring = HumanInputKeyring::from_json(KEYRING_JSON, ACTIVE_KEY_ID)
        .expect("construct IC-020 test keyring");
    let owner = PostgresStore::new_with_lease_config_and_human_input_keyring(
        database_url,
        prefix,
        RunLeaseConfig::new(OWNER_ID, Duration::from_secs(60)).unwrap(),
        Some(keyring.clone()),
    )
    .await
    .expect("open IC-020 owner store");
    let peer = PostgresStore::new_with_lease_config_and_human_input_keyring(
        database_url,
        prefix,
        RunLeaseConfig::new("ic020-store-peer", Duration::from_secs(60)).unwrap(),
        Some(keyring),
    )
    .await
    .expect("open IC-020 peer store");

    assert_eq!(
        owner
            .save_run_intent(run_intent("unkeyed-suggested"))
            .await
            .expect("missing ledger remains a legitimate unkeyed intent"),
        "unkeyed-suggested"
    );

    let active_claim = claim("active-hitl", 'a');
    claim_only(&owner, &active_claim).await;
    owner
        .save_run_intent(run_intent("active-hitl"))
        .await
        .expect("map active IC-020 claim");
    let retained = registration("active-hitl", "retained-question", &active_claim);
    assert_eq!(
        owner.register_human_input(&retained).await.unwrap(),
        HumanInputRegistrationOutcome::Registered
    );

    let drained_claim = claim("drained-claimed", 'b');
    claim_only(&owner, &drained_claim).await;
    assert_eq!(owner.begin_owner_drain().await.unwrap(), 2);

    let error = owner
        .save_run_intent(run_intent("drained-claimed"))
        .await
        .expect_err("drained claim must not create an orphan run");
    assert!(matches!(
        error,
        IronCrewError::OwnerDraining { owner_instance_id } if owner_instance_id == OWNER_ID
    ));

    let rejected = registration("active-hitl", "rejected-question", &active_claim);
    assert_eq!(
        owner.register_human_input(&rejected).await.unwrap(),
        HumanInputRegistrationOutcome::OwnerDraining {
            owner_instance_id: OWNER_ID.into()
        }
    );
    let HumanInputListOutcome::Shared {
        owner_instance_id,
        questions,
    } = peer
        .list_human_inputs(FLOW_NAME, "active-hitl")
        .await
        .unwrap()
    else {
        panic!("pre-drain IC-020 question stopped being observable")
    };
    assert_eq!(owner_instance_id, OWNER_ID);
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].info, retained.question);
    assert_eq!(
        peer.answer_human_input(
            FLOW_NAME,
            "active-hitl",
            "retained-question",
            &serde_json::json!("yes"),
        )
        .await
        .unwrap(),
        HumanInputAnswerOutcome::OwnerDraining {
            owner_instance_id: OWNER_ID.into()
        }
    );

    let pool = sqlx::PgPool::connect(database_url)
        .await
        .expect("connect IC-020 invariant observer");
    let statement = format!(
        "SELECT \
           (SELECT COUNT(*) FROM {p}runs), \
           (SELECT COUNT(*) FROM {p}runs WHERE run_id='drained-claimed'), \
           (SELECT COUNT(*) FROM {p}idempotency), \
           (SELECT COUNT(*) FROM {p}idempotency WHERE owner_draining_at IS NOT NULL), \
           (SELECT COUNT(*) FROM {p}human_inputs), \
           (SELECT COUNT(*) FROM {p}human_inputs WHERE question_id='retained-question' AND state='pending'), \
           (SELECT COUNT(*) FROM {p}human_inputs WHERE question_id='rejected-question'), \
           (SELECT COUNT(*) FROM {p}run_events)",
        p = prefix,
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(statement))
        .fetch_one(&pool)
        .await
        .expect("read IC-020 fail-closed invariants");
    pool.close().await;
    let counts = (0..8)
        .map(|index| row.get::<i64, _>(index))
        .collect::<Vec<_>>();
    assert_eq!(counts, vec![2, 0, 2, 2, 1, 1, 0, 0]);
}

#[tokio::test]
async fn ic020_store_drain_fences_intents_and_human_input_registration() {
    let Some(database_url) = postgres_url() else {
        eprintln!("SKIP IC-020 store drain: IRONCREW_TEST_PG_URL unset");
        return;
    };
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let prefix = format!("p2_020s_{}_", &unique[..12]);
    reset_schema(&database_url, &prefix).await;
    let outcome = AssertUnwindSafe(scenario(&database_url, &prefix))
        .catch_unwind()
        .await;
    let cleanup = AssertUnwindSafe(reset_schema(&database_url, &prefix))
        .catch_unwind()
        .await;
    if let Err(payload) = outcome {
        resume_unwind(payload);
    }
    if let Err(payload) = cleanup {
        resume_unwind(payload);
    }
}

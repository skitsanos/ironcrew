use futures::FutureExt;

use super::*;

const FIXTURE: &str = include_str!("../../fixtures/two_process_replica/ic008_crew.lua");
const CONVERSATION_ID: &str = "ic008-shared-conversation";
const FIRST_CONTENT: &str = "cold-peer-turn";
const FIRST_KEY: &str = "ic008-cold-peer-key-0001";
const DRIFT_KEY: &str = "ic008-definition-drift-key-0002";
const RECOVERY_CONTENT: &str = "recover-after-owner-death";
const RECOVERY_KEY: &str = "ic008-owner-death-key-0003";
const BLOCKING_CONTENT: &str = "block-delete";
const BLOCKING_KEY: &str = "ic008-active-delete-key-0004";

async fn scenario(pair: &mut ProcessPair, provider: ic008_provider::ProviderProbe) {
    let owner_id = pair.owner_a_id.clone();
    let peer_id = ic008_http::instance_id(pair, &pair.survivor_b.base_url).await;
    assert_ne!(owner_id, peer_id);

    let started = ic008_http::start(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await;
    ic008_assert::execution_identity(&started);
    let peer_started =
        ic008_http::start(pair, &pair.survivor_b.base_url, &peer_id, CONVERSATION_ID).await;
    assert_eq!(peer_started, started, "IC-008 peer /start changed identity");
    provider.assert_stable_count(0).await;

    ic008_http::assert_error(
        ic008_http::message(
            pair,
            &pair.survivor_b.base_url,
            CONVERSATION_ID,
            "missing-key",
            None,
        )
        .await,
        &peer_id,
        StatusCode::BAD_REQUEST,
        "Validation error: Idempotency-Key is required for this endpoint",
    )
    .await;
    provider.assert_stable_count(0).await;

    let first = ic008_http::successful_message(
        ic008_http::message(
            pair,
            &pair.survivor_b.base_url,
            CONVERSATION_ID,
            FIRST_CONTENT,
            Some(FIRST_KEY),
        )
        .await,
        &peer_id,
        false,
        FIRST_CONTENT,
    )
    .await;
    ic008_assert::message_identity(&first, &started);
    assert_eq!(provider.request_count(), 1);

    let owner_id =
        ic008_process::restart_owner_with_message_limit(pair, &provider.base_url, "1").await;
    let replay = ic008_http::successful_message(
        ic008_http::message(
            pair,
            &pair.owner_a.base_url,
            CONVERSATION_ID,
            FIRST_CONTENT,
            Some(FIRST_KEY),
        )
        .await,
        &owner_id,
        true,
        FIRST_CONTENT,
    )
    .await;
    assert_eq!(replay, first, "IC-008 replay body changed");
    provider.assert_stable_count(1).await;

    let history_a =
        ic008_http::history(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await;
    let history_b =
        ic008_http::history(pair, &pair.survivor_b.base_url, &peer_id, CONVERSATION_ID).await;
    assert_eq!(history_a, history_b);
    ic008_assert::history(
        &history_a,
        &started,
        &first,
        CONVERSATION_ID,
        1,
        &[FIRST_CONTENT, "mock:cold-peer-turn"],
    );

    let sse_a =
        ic008_http::sse_conflict(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await;
    let sse_b =
        ic008_http::sse_conflict(pair, &pair.survivor_b.base_url, &peer_id, CONVERSATION_ID).await;
    assert_eq!(sse_a, sse_b, "IC-008 replicas disagreed about SSE");

    let flow_file = pair._workspace.path().join(FLOW).join("crew.lua");
    std::fs::write(
        &flow_file,
        format!("{FIXTURE}\n-- IC-008 deliberate drift\n"),
    )
    .expect("write IC-008 drift fixture");
    ic008_http::assert_error(
        ic008_http::message(
            pair,
            &pair.survivor_b.base_url,
            CONVERSATION_ID,
            "must-not-reach-provider",
            Some(DRIFT_KEY),
        )
        .await,
        &peer_id,
        StatusCode::CONFLICT,
        "Conversation flow source changed; restore the original definition or start a new conversation",
    )
    .await;
    std::fs::write(&flow_file, FIXTURE).expect("restore IC-008 fixture");
    provider.assert_stable_count(1).await;

    // A replacement with a different effective provider endpoint must not
    // resume the durable definition. This is a cold process boundary: the
    // unreachable endpoint proves rejection happens before provider work.
    let drifted_peer =
        ic008_process::restart_peer_as(pair, "http://127.0.0.1:9/v1", "provider-drift").await;
    ic008_http::assert_error(
        ic008_http::start_response(pair, &pair.survivor_b.base_url, CONVERSATION_ID).await,
        &drifted_peer,
        StatusCode::CONFLICT,
        "Conflict: Conversation definition changed; restore the original model, agent, tools, provider, and limits or start a new conversation",
    )
    .await;
    provider.assert_stable_count(1).await;

    let peer_id = ic008_process::restart_peer(pair, &provider.base_url).await;
    assert_eq!(
        ic008_http::instance_id(pair, &pair.survivor_b.base_url).await,
        peer_id
    );
    let owner_port = ic008_process::kill_owner(pair);

    let recovered = ic008_http::successful_message(
        ic008_http::message(
            pair,
            &pair.survivor_b.base_url,
            CONVERSATION_ID,
            RECOVERY_CONTENT,
            Some(RECOVERY_KEY),
        )
        .await,
        &peer_id,
        false,
        RECOVERY_CONTENT,
    )
    .await;
    ic008_assert::message_identity(&recovered, &started);
    assert_eq!(provider.request_count(), 2);

    let owner_id = ic008_process::replace_owner(pair, owner_port, &provider.base_url).await;
    let resumed = ic008_http::start(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await;
    assert_eq!(resumed["revision"], recovered["revision"]);
    assert_eq!(resumed["incarnation_id"], started["incarnation_id"]);
    let recovered_a =
        ic008_http::history(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await;
    let recovered_b =
        ic008_http::history(pair, &pair.survivor_b.base_url, &peer_id, CONVERSATION_ID).await;
    assert_eq!(recovered_a, recovered_b);
    ic008_assert::history(
        &recovered_a,
        &started,
        &recovered,
        CONVERSATION_ID,
        2,
        &[FIRST_CONTENT, RECOVERY_CONTENT],
    );

    let blocked = ic008_http::spawn_message(
        pair,
        pair.survivor_b.base_url.clone(),
        CONVERSATION_ID,
        BLOCKING_CONTENT,
        BLOCKING_KEY,
    );
    provider.wait_until_blocked(3).await;
    ic008_sql::wait_for_active_turn(pair, CONVERSATION_ID).await;
    ic008_http::assert_error_contains(
        ic008_http::message(
            pair,
            &pair.owner_a.base_url,
            CONVERSATION_ID,
            BLOCKING_CONTENT,
            Some(BLOCKING_KEY),
        )
        .await,
        &owner_id,
        StatusCode::CONFLICT,
        "already in progress",
    )
    .await;
    provider.assert_stable_count(3).await;
    let same_replica_delete = tokio::time::timeout(
        Duration::from_secs(3),
        ic008_http::delete(pair, &pair.survivor_b.base_url, &peer_id, CONVERSATION_ID),
    )
    .await
    .expect("IC-008 same-replica active delete did not fail fast");
    ic008_http::assert_error_contains(
        same_replica_delete,
        &peer_id,
        StatusCode::CONFLICT,
        "Conversation is busy",
    )
    .await;
    ic008_http::assert_error_contains(
        ic008_http::delete(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await,
        &owner_id,
        StatusCode::CONFLICT,
        "active idempotent message operation",
    )
    .await;
    provider.release_blocked();
    let blocked_response = tokio::time::timeout(Duration::from_secs(15), blocked)
        .await
        .expect("IC-008 blocked turn deadline")
        .expect("join IC-008 blocked turn");
    let third =
        ic008_http::successful_message(blocked_response, &peer_id, false, BLOCKING_CONTENT).await;
    ic008_assert::message_identity(&third, &started);
    let concurrent_replay = ic008_http::successful_message(
        ic008_http::message(
            pair,
            &pair.owner_a.base_url,
            CONVERSATION_ID,
            BLOCKING_CONTENT,
            Some(BLOCKING_KEY),
        )
        .await,
        &owner_id,
        true,
        BLOCKING_CONTENT,
    )
    .await;
    assert_eq!(concurrent_replay, third);
    provider.assert_stable_count(3).await;

    ic008_assert::deleted(
        ic008_http::delete(pair, &pair.survivor_b.base_url, &peer_id, CONVERSATION_ID).await,
        CONVERSATION_ID,
    )
    .await;
    let recreated =
        ic008_http::start(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await;
    ic008_assert::execution_identity(&recreated);
    assert_ne!(recreated["incarnation_id"], started["incarnation_id"]);
    assert_eq!(
        recreated["source_fingerprint"],
        started["source_fingerprint"]
    );
    assert_eq!(
        recreated["definition_fingerprint"],
        started["definition_fingerprint"]
    );
    ic008_http::assert_error(
        ic008_http::message(
            pair,
            &pair.survivor_b.base_url,
            CONVERSATION_ID,
            FIRST_CONTENT,
            Some(FIRST_KEY),
        )
        .await,
        &peer_id,
        StatusCode::CONFLICT,
        "Idempotency-Key was already used for a different request",
    )
    .await;
    provider.assert_stable_count(3).await;
    ic008_assert::deleted(
        ic008_http::delete(pair, &pair.owner_a.base_url, &owner_id, CONVERSATION_ID).await,
        CONVERSATION_ID,
    )
    .await;
    ic008_sql::assert_final_state(pair, 3).await;
}

#[tokio::test]
async fn ic008_shared_conversation_coordination_is_truthful() {
    let provider = ic008_provider::MockProvider::start(BLOCKING_CONTENT).await;
    let probe = provider.probe();
    let env = [
        ("IRONCREW_ALLOW_PRIVATE_IPS", "1"),
        ("IRONCREW_ENV_ALLOWLIST", "IC008_PROVIDER_BASE_URL"),
        ("IC008_PROVIDER_BASE_URL", probe.base_url.as_str()),
        ("IRONCREW_MAX_CONVERSATION_TURN_SECS", "10"),
    ];
    with_configured_process_pair("008", true, FIXTURE, &env, |pair| {
        let probe = probe.clone();
        async move { scenario(pair, probe).await }.boxed_local()
    })
    .await;
}

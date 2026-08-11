use super::*;

use super::ic016_http::{
    answer_once, assert_durable_sse_hides, assert_raw_logs_hide, assert_unsafe_routes_fail,
    list_once,
};
use super::ic016_process::{
    INITIAL_ENV, NEW_KEY_MATERIAL, NEW_ONLY, OLD_KEY_MATERIAL, OVERLAP_NEW_ACTIVE,
    OVERLAP_OLD_ACTIVE, assert_unsafe_peer_rejected, keyring_fingerprints, replace_rejected_peer,
    restart_owner, restart_peer, restart_peer_without_readiness,
};
use super::ic016_sql::{
    assert_answered, assert_consumed_once, assert_no_old_references, assert_shared_sql_hides,
    assert_unchanged, wait_for_pending,
};

const FIXTURE: &str = include_str!("../../fixtures/two_process_replica/ic016_crew.lua");
const PROMPT: &str = "Approve the IC-016 key rotation?";
const ANSWER: &str = "rotation-approved";

async fn finish_answer(
    pair: &mut ProcessPair,
    run_id: &str,
    question_id: &str,
    question_fingerprint: &str,
    answer_fingerprint: &str,
    forbidden: &[&str],
) {
    let _lease = wait_for_next_lease(pair, run_id).await;
    pair.owner_a.suspend_and_wait();
    answer_once(pair, run_id, question_id, ANSWER).await;
    assert_answered(
        pair,
        run_id,
        question_fingerprint,
        answer_fingerprint,
        PROMPT,
        ANSWER,
    )
    .await;
    pair.owner_a.resume();
    assert_consumed_once(pair, run_id).await;
    assert_durable_sse_hides(pair, run_id, forbidden).await;
    assert_shared_sql_hides(pair, run_id, forbidden).await;
}

async fn old_only_to_overlap(pair: &mut ProcessPair, old_fingerprint: &str, forbidden: &[&str]) {
    let started = start_keyed_run(pair, "ic016-old-answer-key-0001").await;
    let run_id = started["run_id"].as_str().expect("IC-016 old run id");
    let pending = wait_for_pending(pair, run_id, old_fingerprint, PROMPT).await;

    restart_peer_without_readiness(pair, NEW_ONLY);
    let listener_live = assert_unsafe_peer_rejected(pair).await;
    assert_unsafe_routes_fail(pair, run_id, &pending.question_id, listener_live, forbidden).await;
    assert_unchanged(pair, run_id, &pending).await;

    replace_rejected_peer(pair, OVERLAP_OLD_ACTIVE).await;
    let question_id = list_once(pair, run_id, PROMPT).await;
    assert_eq!(question_id, pending.question_id);
    finish_answer(
        pair,
        run_id,
        &question_id,
        old_fingerprint,
        old_fingerprint,
        forbidden,
    )
    .await;
    restart_owner(pair, OVERLAP_OLD_ACTIVE).await;
}

async fn roll_new_active(pair: &mut ProcessPair, old_fingerprint: &str, forbidden: &[&str]) {
    let started = start_keyed_run(pair, "ic016-new-answer-key-0002").await;
    let run_id = started["run_id"].as_str().expect("IC-016 overlap run id");
    let pending = wait_for_pending(pair, run_id, old_fingerprint, PROMPT).await;
    restart_peer(pair, OVERLAP_NEW_ACTIVE).await;
    let question_id = list_once(pair, run_id, PROMPT).await;
    assert_eq!(question_id, pending.question_id);
    finish_answer(
        pair,
        run_id,
        &question_id,
        old_fingerprint,
        old_fingerprint,
        forbidden,
    )
    .await;
    restart_owner(pair, OVERLAP_NEW_ACTIVE).await;
}

async fn remove_old_key(
    pair: &mut ProcessPair,
    old_fingerprint: &str,
    new_fingerprint: &str,
    forbidden: &[&str],
) {
    let started = start_keyed_run(pair, "ic016-new-only-key-0003").await;
    let run_id = started["run_id"].as_str().expect("IC-016 new run id");
    let pending = wait_for_pending(pair, run_id, new_fingerprint, PROMPT).await;
    assert_no_old_references(pair, old_fingerprint).await;

    restart_peer(pair, NEW_ONLY).await;
    let question_id = list_once(pair, run_id, PROMPT).await;
    assert_eq!(question_id, pending.question_id);
    finish_answer(
        pair,
        run_id,
        &question_id,
        new_fingerprint,
        new_fingerprint,
        forbidden,
    )
    .await;
    assert_no_old_references(pair, old_fingerprint).await;
    restart_owner(pair, NEW_ONLY).await;
    assert_no_old_references(pair, old_fingerprint).await;
}

async fn scenario(pair: &mut ProcessPair) {
    let (old_fingerprint, new_fingerprint) = keyring_fingerprints();
    let forbidden = [PROMPT, ANSWER, OLD_KEY_MATERIAL, NEW_KEY_MATERIAL];

    old_only_to_overlap(pair, &old_fingerprint, &forbidden).await;
    roll_new_active(pair, &old_fingerprint, &forbidden).await;
    remove_old_key(pair, &old_fingerprint, &new_fingerprint, &forbidden).await;
    pair.stop();
    assert_raw_logs_hide(pair, &forbidden);
}

#[tokio::test]
async fn mixed_revision_human_input_key_rotation_is_fail_closed_and_exact_once() {
    with_configured_process_pair("ic016", true, FIXTURE, INITIAL_ENV, |pair| {
        Box::pin(scenario(pair))
    })
    .await;
}

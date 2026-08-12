use reqwest::header::{CACHE_CONTROL, RETRY_AFTER};

use super::super::*;

pub(super) const INSTANCE_HEADER: &str = "x-ironcrew-instance-id";

pub(super) fn assert_receiver(response: &reqwest::Response, expected: &str) {
    assert_eq!(
        response
            .headers()
            .get(INSTANCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(expected),
        "IC-020 response receiver attribution"
    );
}

async fn capabilities(pair: &ProcessPair, base_url: &str) -> (String, serde_json::Value) {
    let response = authenticated(pair.client.get(format!("{base_url}/capabilities")))
        .send()
        .await
        .expect("query IC-020 capabilities");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let receiver = response.headers()[INSTANCE_HEADER]
        .to_str()
        .expect("IC-020 capability receiver")
        .to_owned();
    let body = response.json().await.expect("parse IC-020 capabilities");
    (receiver, body)
}

pub(super) async fn assert_accepting_topology(pair: &ProcessPair) -> String {
    for base_url in [&pair.owner_a.base_url, &pair.survivor_b.base_url] {
        let readiness = pair
            .client
            .get(format!("{base_url}/health/ready"))
            .send()
            .await
            .expect("query IC-020 accepting readiness");
        assert_eq!(readiness.status(), StatusCode::OK);
    }
    let (owner_receiver, owner) = capabilities(pair, &pair.owner_a.base_url).await;
    let (peer_receiver, peer) = capabilities(pair, &pair.survivor_b.base_url).await;
    assert_eq!(owner_receiver, pair.owner_a_id);
    assert_eq!(owner["instance_id"], pair.owner_a_id);
    assert_eq!(owner["lifecycle_state"], "accepting");
    assert_eq!(peer["instance_id"], peer_receiver);
    assert_eq!(peer["lifecycle_state"], "accepting");
    assert_ne!(owner_receiver, peer_receiver);
    peer_receiver
}

pub(super) async fn start_keyed(
    pair: &ProcessPair,
    base_url: &str,
    receiver: &str,
    key: &str,
) -> serde_json::Value {
    let response = authenticated(pair.client.post(format!("{base_url}/flows/{FLOW}/run")))
        .header("Idempotency-Key", key)
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("start IC-020 keyed run");
    assert_eq!(response.status(), StatusCode::OK);
    assert_receiver(&response, receiver);
    assert!(response.headers().get("Idempotency-Replayed").is_none());
    let body: serde_json::Value = response.json().await.expect("parse IC-020 run acceptance");
    assert_eq!(body["owner_instance_id"], receiver);
    body
}

async fn peer_readiness_is_healthy(pair: &ProcessPair, context: &str) -> bool {
    let response = pair
        .client
        .get(format!("{}/health/ready", pair.survivor_b.base_url))
        .send()
        .await
        .unwrap_or_else(|error| panic!("query IC-020 peer readiness during {context}: {error}"));
    if response.status() == StatusCode::OK {
        return true;
    }
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "unexpected IC-020 peer readiness status during {context}"
    );
    let body: serde_json::Value = response
        .json()
        .await
        .expect("parse IC-020 peer maintenance readiness");
    assert_eq!(
        body["status"], "not_ready",
        "unexpected IC-020 peer readiness body during {context}"
    );
    assert_eq!(
        body["component"], "storage_maintenance",
        "unexpected IC-020 peer readiness component during {context}"
    );
    false
}

pub(super) async fn wait_draining(pair: &ProcessPair, peer_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // The global run-fence deliberately contends with the peer's bounded
        // maintenance cycle. Its exact storage-maintenance 503 is truthful;
        // require recovery before declaring the owner fully drained.
        let peer_ready = peer_readiness_is_healthy(pair, "explicit drain").await;
        let response = pair
            .client
            .get(format!("{}/health/ready", pair.owner_a.base_url))
            .send()
            .await
            .expect("query IC-020 draining readiness");
        if response.status() == StatusCode::SERVICE_UNAVAILABLE {
            let body: serde_json::Value = response
                .json()
                .await
                .expect("parse IC-020 draining readiness");
            if peer_ready
                && body["component"] == "lifecycle"
                && body["lifecycle_state"] == "draining"
            {
                assert_eq!(body["status"], "not_ready");
                let (receiver, capability) = capabilities(pair, &pair.owner_a.base_url).await;
                assert_eq!(receiver, pair.owner_a_id);
                assert_eq!(capability["lifecycle_state"], "draining");
                assert_ne!(receiver, peer_id);
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "IC-020 owner never entered draining state\n{}",
        pair.owner_a.logs()
    );
}

pub(super) async fn wait_fencing(pair: &ProcessPair, peer_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // Do not require recovery while the test itself still owns the global
        // advisory lock; only the exact bounded-maintenance response is valid.
        let _peer_ready = peer_readiness_is_healthy(pair, "owner fencing").await;
        let owner = pair
            .client
            .get(format!("{}/health/ready", pair.owner_a.base_url))
            .send()
            .await
            .expect("query IC-020 fencing readiness");
        if owner.status() == StatusCode::SERVICE_UNAVAILABLE {
            let body: serde_json::Value = owner.json().await.expect("parse IC-020 fencing body");
            if body["lifecycle_state"] == "fencing" {
                assert_eq!(body["status"], "not_ready");
                assert_eq!(body["component"], "lifecycle");
                let (receiver, capability) = capabilities(pair, &pair.owner_a.base_url).await;
                assert_eq!(receiver, pair.owner_a_id);
                assert_eq!(capability["lifecycle_state"], "fencing");
                assert_ne!(receiver, peer_id);
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("IC-020 owner never exposed fencing readiness");
}

pub(super) async fn assert_fencing_work_rejected(pair: &ProcessPair) {
    let response = authenticated(
        pair.client
            .post(format!("{}/flows/{FLOW}/run", pair.owner_a.base_url)),
    )
    .header("Idempotency-Key", "ic020-fencing-new-key-0004")
    .json(&serde_json::json!({}))
    .send()
    .await
    .expect("send IC-020 work while owner fence is blocked");
    let body =
        assert_draining_rejection(response, &pair.owner_a_id, "instance_draining", true).await;
    assert_eq!(body["lifecycle_state"], "fencing");
    assert_eq!(body["instance_id"], pair.owner_a_id);
}

async fn assert_draining_rejection(
    response: reqwest::Response,
    receiver: &str,
    code: &str,
    require_retry_after: bool,
) -> serde_json::Value {
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_receiver(&response, receiver);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    if require_retry_after {
        let retry_after = response.headers()[RETRY_AFTER]
            .to_str()
            .expect("IC-020 Retry-After text")
            .parse::<u64>()
            .expect("IC-020 numeric Retry-After");
        assert!(retry_after >= 1);
    }
    let body: serde_json::Value = response.json().await.expect("parse IC-020 drain rejection");
    assert_eq!(body["code"], code, "unexpected IC-020 rejection: {body}");
    body
}

pub(super) async fn assert_mutations_rejected(
    pair: &ProcessPair,
    peer_id: &str,
    run_id: &str,
    question_id: &str,
) {
    let owner = &pair.owner_a.base_url;
    let direct = [
        authenticated(pair.client.post(format!("{owner}/flows/{FLOW}/run")))
            .header("Idempotency-Key", "ic020-draining-new-key-0002")
            .json(&serde_json::json!({})),
        authenticated(
            pair.client
                .post(format!("{owner}/flows/{FLOW}/abort/{run_id}")),
        ),
        authenticated(
            pair.client
                .post(format!("{owner}/flows/{FLOW}/answer/{run_id}")),
        )
        .json(&serde_json::json!({
            "question_id": question_id,
            "answer": "must-not-queue"
        })),
        authenticated(
            pair.client
                .delete(format!("{owner}/flows/{FLOW}/runs/{run_id}")),
        ),
    ];
    for request in direct {
        let response = request.send().await.expect("send IC-020 direct mutation");
        let body =
            assert_draining_rejection(response, &pair.owner_a_id, "instance_draining", true).await;
        assert_eq!(body["lifecycle_state"], "draining");
        assert_eq!(body["instance_id"], pair.owner_a_id);
    }

    assert_peer_controls_rejected(pair, peer_id, run_id, question_id).await;
}

pub(super) async fn assert_peer_controls_rejected(
    pair: &ProcessPair,
    peer_id: &str,
    run_id: &str,
    question_id: &str,
) {
    let peer = &pair.survivor_b.base_url;
    let abort = authenticated(
        pair.client
            .post(format!("{peer}/flows/{FLOW}/abort/{run_id}")),
    )
    .send()
    .await
    .expect("send IC-020 peer abort");
    let abort = assert_draining_rejection(abort, peer_id, "run_owner_draining", true).await;
    assert_eq!(abort["run_id"], run_id);
    assert_eq!(abort["owner_instance_id"], pair.owner_a_id);
    assert_eq!(abort["control_scope"], "shared_store");
    let answer = authenticated(
        pair.client
            .post(format!("{peer}/flows/{FLOW}/answer/{run_id}")),
    )
    .json(&serde_json::json!({
        "question_id": question_id,
        "answer": "must-not-queue"
    }))
    .send()
    .await
    .expect("send IC-020 peer answer");
    let answer = assert_draining_rejection(answer, peer_id, "run_owner_draining", true).await;
    assert_eq!(answer["run_id"], run_id);
    assert_eq!(answer["owner_instance_id"], pair.owner_a_id);
    assert_eq!(answer["control_scope"], "shared_store");
}

use super::*;

pub(super) const OLD_KEY_ID: &str = "rotation-old-v1";
pub(super) const NEW_KEY_ID: &str = "rotation-new-v2";
pub(super) const OLD_KEY_MATERIAL: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
pub(super) const NEW_KEY_MATERIAL: &str = "AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=";

const OLD_ONLY_JSON: &str = r#"{"rotation-old-v1":"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="}"#;
const OVERLAP_JSON: &str = r#"{"rotation-old-v1":"AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=","rotation-new-v2":"AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI="}"#;
const NEW_ONLY_JSON: &str = r#"{"rotation-new-v2":"AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI="}"#;

#[derive(Clone, Copy)]
pub(super) struct Revision {
    pub(super) label: &'static str,
    pub(super) keyring: &'static str,
    pub(super) active_key: &'static str,
}

pub(super) const OVERLAP_OLD_ACTIVE: Revision = Revision {
    label: "overlap-old-active",
    keyring: OVERLAP_JSON,
    active_key: OLD_KEY_ID,
};
pub(super) const OVERLAP_NEW_ACTIVE: Revision = Revision {
    label: "overlap-new-active",
    keyring: OVERLAP_JSON,
    active_key: NEW_KEY_ID,
};
pub(super) const NEW_ONLY: Revision = Revision {
    label: "new-only",
    keyring: NEW_ONLY_JSON,
    active_key: NEW_KEY_ID,
};

pub(super) const INITIAL_ENV: &[(&str, &str)] = &[
    ("IRONCREW_HITL_ENCRYPTION_KEYS", OLD_ONLY_JSON),
    ("IRONCREW_HITL_ACTIVE_KEY_ID", OLD_KEY_ID),
    ("IRONCREW_HITL_POLL_INTERVAL_MS", "500"),
];

fn port(process: &ReplicaProcess) -> u16 {
    url::Url::parse(&process.base_url)
        .expect("parse IC-016 replica URL")
        .port()
        .expect("IC-016 replica URL port")
}

fn spawn_revision(pair: &ProcessPair, side: &str, port: u16, revision: Revision) -> ReplicaProcess {
    let instance_id = format!("ic016-{side}-{}", revision.label);
    let extra_env = [
        ("IRONCREW_HITL_ENCRYPTION_KEYS", revision.keyring),
        ("IRONCREW_HITL_ACTIVE_KEY_ID", revision.active_key),
        ("IRONCREW_HITL_POLL_INTERVAL_MS", "500"),
    ];
    ReplicaProcess::spawn_with_policy(
        &instance_id,
        &instance_id,
        port,
        pair._workspace.path(),
        &pair.database_url,
        &pair.prefix,
        pair._workspace.path(),
        true,
        &extra_env,
    )
}

pub(super) async fn restart_owner(pair: &mut ProcessPair, revision: Revision) {
    let owner_port = port(&pair.owner_a);
    let status = pair.owner_a.shutdown();
    assert!(status.success(), "IC-016 owner shutdown failed: {status}");
    let mut replacement = spawn_revision(pair, "owner", owner_port, revision);
    replacement.wait_until_ready(&pair.client).await;
    pair.owner_a_id = format!("ic016-owner-{}", revision.label);
    pair.owner_a = replacement;
}

pub(super) async fn restart_peer(pair: &mut ProcessPair, revision: Revision) {
    let peer_port = port(&pair.survivor_b);
    stop_peer(pair);
    let mut replacement = spawn_revision(pair, "peer", peer_port, revision);
    replacement.wait_until_ready(&pair.client).await;
    pair.survivor_b = replacement;
}

pub(super) fn restart_peer_without_readiness(pair: &mut ProcessPair, revision: Revision) {
    let peer_port = port(&pair.survivor_b);
    stop_peer(pair);
    pair.survivor_b = spawn_revision(pair, "peer-unsafe", peer_port, revision);
}

pub(super) async fn assert_unsafe_peer_rejected(pair: &mut ProcessPair) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(status) = pair
            .survivor_b
            .child
            .try_wait()
            .expect("inspect IC-016 unsafe replica")
        {
            assert!(
                !status.success(),
                "IC-016 unsafe key removal exited successfully"
            );
            return false;
        }
        let live = pair
            .client
            .get(format!("{}/health/live", pair.survivor_b.base_url))
            .send()
            .await;
        if live.is_ok_and(|response| response.status() == StatusCode::OK) {
            let response = pair
                .client
                .get(format!("{}/health/ready", pair.survivor_b.base_url))
                .send()
                .await
                .expect("read IC-016 unsafe readiness");
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body: serde_json::Value = response
                .json()
                .await
                .expect("parse IC-016 unsafe readiness");
            assert_eq!(body["status"], "not_ready");
            assert_eq!(body["component"], "storage");
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "IC-016 unsafe key-removal replica neither exited nor served a fail-closed probe\n{}",
        pair.survivor_b.logs()
    );
}

pub(super) async fn replace_rejected_peer(pair: &mut ProcessPair, revision: Revision) {
    let peer_port = port(&pair.survivor_b);
    let _ = pair.survivor_b.shutdown();
    let mut replacement = spawn_revision(pair, "peer", peer_port, revision);
    replacement.wait_until_ready(&pair.client).await;
    pair.survivor_b = replacement;
}

pub(super) fn stop_peer(pair: &mut ProcessPair) {
    let status = pair.survivor_b.shutdown();
    assert!(
        status.success(),
        "IC-016 peer shutdown failed: {status}\n{}",
        pair.survivor_b.logs()
    );
}

pub(super) fn keyring_fingerprints() -> (String, String) {
    use ironcrew::engine::human_input::HumanInputKeyring;

    let old = HumanInputKeyring::from_json(OLD_ONLY_JSON, OLD_KEY_ID)
        .expect("build IC-016 old keyring")
        .active_fingerprint()
        .to_owned();
    let new = HumanInputKeyring::from_json(NEW_ONLY_JSON, NEW_KEY_ID)
        .expect("build IC-016 new keyring")
        .active_fingerprint()
        .to_owned();
    assert_ne!(old, new);
    (old, new)
}

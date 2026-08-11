use super::*;

fn port(process: &ReplicaProcess) -> u16 {
    url::Url::parse(&process.base_url)
        .expect("parse IC-008 replica URL")
        .port()
        .expect("IC-008 replica port")
}

fn spawn(
    pair: &ProcessPair,
    name: &str,
    instance_id: &str,
    port: u16,
    provider_base_url: &str,
    message_max_bytes: Option<&str>,
) -> ReplicaProcess {
    let mut extra_env = vec![
        ("IRONCREW_ALLOW_PRIVATE_IPS", "1"),
        ("IRONCREW_ENV_ALLOWLIST", "IC008_PROVIDER_BASE_URL"),
        ("IC008_PROVIDER_BASE_URL", provider_base_url),
        ("IRONCREW_MAX_CONVERSATION_TURN_SECS", "10"),
    ];
    if let Some(limit) = message_max_bytes {
        extra_env.push(("IRONCREW_API_MESSAGE_MAX_BYTES", limit));
    }
    ReplicaProcess::spawn_with_policy(
        name,
        instance_id,
        port,
        pair._workspace.path(),
        &pair.database_url,
        &pair.prefix,
        pair._workspace.path(),
        true,
        &extra_env,
    )
}

pub(super) async fn restart_peer_as(
    pair: &mut ProcessPair,
    provider_base_url: &str,
    generation: &str,
) -> String {
    let peer_port = port(&pair.survivor_b);
    let status = pair.survivor_b.shutdown();
    assert!(
        status.success(),
        "IC-008 peer rolling restart failed: {status}\n{}",
        pair.survivor_b.logs()
    );
    let instance_id = format!("ic008-{generation}-{}", pair.prefix.trim_matches('_'));
    let mut replacement = spawn(
        pair,
        "ic008-peer-replacement",
        &instance_id,
        peer_port,
        provider_base_url,
        None,
    );
    replacement.wait_until_ready(&pair.client).await;
    pair.survivor_b = replacement;
    instance_id
}

pub(super) async fn restart_owner_with_message_limit(
    pair: &mut ProcessPair,
    provider_base_url: &str,
    limit: &str,
) -> String {
    let owner_port = port(&pair.owner_a);
    let status = pair.owner_a.shutdown();
    assert!(status.success(), "IC-008 owner restart failed: {status}");
    let instance_id = format!("ic008-message-limit-{}", pair.prefix.trim_matches('_'));
    let mut replacement = spawn(
        pair,
        "ic008-owner-message-limit",
        &instance_id,
        owner_port,
        provider_base_url,
        Some(limit),
    );
    replacement.wait_until_ready(&pair.client).await;
    pair.owner_a_id.clone_from(&instance_id);
    pair.owner_a = replacement;
    instance_id
}

pub(super) async fn restart_peer(pair: &mut ProcessPair, provider_base_url: &str) -> String {
    restart_peer_as(pair, provider_base_url, "b2").await
}

pub(super) fn kill_owner(pair: &mut ProcessPair) -> u16 {
    let owner_port = port(&pair.owner_a);
    assert_sigkill(&mut pair.owner_a);
    owner_port
}

pub(super) async fn replace_owner(
    pair: &mut ProcessPair,
    owner_port: u16,
    provider_base_url: &str,
) -> String {
    let instance_id = format!("ic008-a2-{}", pair.prefix.trim_matches('_'));
    let mut replacement = spawn(
        pair,
        "ic008-owner-replacement",
        &instance_id,
        owner_port,
        provider_base_url,
        None,
    );
    replacement.wait_until_ready(&pair.client).await;
    pair.owner_a_id.clone_from(&instance_id);
    pair.owner_a = replacement;
    instance_id
}

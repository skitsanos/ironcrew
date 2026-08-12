use super::super::*;

fn port(process: &ReplicaProcess) -> u16 {
    url::Url::parse(&process.base_url)
        .expect("parse IC-020 replica URL")
        .port()
        .expect("IC-020 replica URL port")
}

pub(super) fn begin_explicit_drain(process: &mut ReplicaProcess) {
    let pid = nix::unistd::Pid::from_raw(process.child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGUSR1)
        .expect("send SIGUSR1 to IC-020 replica");
}

pub(super) async fn shutdown_cleanly(
    process: &mut ReplicaProcess,
    client: &Client,
    peer_base_url: &str,
    label: &str,
) {
    let sigterm_sent_at = signal_terminate(process);
    wait_clean_exit(process, client, peer_base_url, sigterm_sent_at, label).await;
}

pub(super) fn signal_terminate(process: &mut ReplicaProcess) -> Instant {
    let sent_at = Instant::now();
    let pid = nix::unistd::Pid::from_raw(process.child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to IC-020 replica");
    sent_at
}

pub(super) fn assert_alive(process: &mut ReplicaProcess, label: &str) {
    assert!(
        process
            .child
            .try_wait()
            .expect("inspect IC-020 replica liveness")
            .is_none(),
        "{label} exited before its durable owner fence committed\n{}",
        process.logs()
    );
}

pub(super) async fn wait_clean_exit(
    process: &mut ReplicaProcess,
    client: &Client,
    peer_base_url: &str,
    sigterm_sent_at: Instant,
    label: &str,
) {
    let deadline = sigterm_sent_at + Duration::from_secs(8);
    loop {
        let peer = client
            .get(format!("{peer_base_url}/health/ready"))
            .send()
            .await
            .expect("query IC-020 peer readiness during shutdown");
        assert_eq!(
            peer.status(),
            StatusCode::OK,
            "IC-020 peer lost readiness during {label}"
        );
        if let Some(status) = process
            .child
            .try_wait()
            .expect("inspect IC-020 draining replica")
        {
            assert!(
                status.success(),
                "{label} did not shut down cleanly with SIGTERM: {status}\n{}",
                process.logs()
            );
            assert!(
                !process
                    .logs()
                    .contains("Graceful shutdown exceeded its teardown deadline"),
                "{label} exited through the teardown-timeout fallback\n{}",
                process.logs()
            );
            assert!(sigterm_sent_at.elapsed() < Duration::from_secs(8));
            return;
        }
        if Instant::now() >= deadline {
            let _ = process.child.kill();
            let _ = process.child.wait();
            panic!("{label} exceeded the bounded IC-020 shutdown window");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(super) async fn replace_owner(pair: &mut ProcessPair, extra_env: &[(&str, &str)]) -> String {
    let owner_port = port(&pair.owner_a);
    let instance_id = format!("ic020-c-{}", pair.prefix.trim_matches('_'));
    let mut replacement = ReplicaProcess::spawn_with_policy(
        "ic020-replacement-c",
        &instance_id,
        owner_port,
        pair._workspace.path(),
        &pair.database_url,
        &pair.prefix,
        pair._workspace.path(),
        true,
        extra_env,
    );
    replacement.wait_until_ready(&pair.client).await;
    pair.owner_a_id.clone_from(&instance_id);
    pair.owner_a = replacement;
    instance_id
}

use super::*;

#[test]
fn timeout_must_be_positive_and_capped() {
    let policy = ShellPolicy::capture();
    assert!(
        policy
            .requested_timeout(&json!({"timeout_secs": 0}))
            .is_err()
    );
    assert!(
        policy
            .requested_timeout(&json!({"timeout_secs": MAX_TIMEOUT_SECS + 1}))
            .is_err()
    );
    assert!(
        policy
            .requested_timeout(&json!({"timeout_secs": 1.5}))
            .is_err()
    );
    assert_eq!(
        policy
            .requested_timeout(&json!({"timeout_secs": 2}))
            .unwrap(),
        Duration::from_secs(2)
    );
}

#[tokio::test]
async fn command_size_is_bounded_before_spawn() {
    let error = ShellTool::new()
        .execute(
            json!({"command": "x".repeat(HARD_MAX_COMMAND_BYTES + 1)}),
            &ToolCallContext::default(),
        )
        .await
        .expect_err("oversized command must fail");
    assert!(error.to_string().contains("65536"));
}

#[tokio::test]
async fn timeout_terminates_background_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("descendant-survived");
    let marker = marker.to_string_lossy().replace('\'', "'\\''");
    let command = format!("(sleep 2; echo leaked > '{marker}') & wait");

    let error = ShellTool::new()
        .execute(
            json!({"command": command, "timeout_secs": 1}),
            &ToolCallContext::default(),
        )
        .await
        .expect_err("command must time out");
    assert!(error.to_string().contains("timed out"));

    tokio::time::sleep(Duration::from_millis(1_300)).await;
    assert!(
        !dir.path().join("descendant-survived").exists(),
        "background child survived process-group cleanup"
    );
}

#[tokio::test]
async fn command_cannot_read_provider_keys_from_the_process_environment() {
    // SAFETY: a uniquely named variable no other test reads or writes.
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-must-not-leak");
    }

    let output = ShellTool::new()
        .execute(
            json!({"command": "echo \"key=[${OPENAI_API_KEY:-absent}]\"; env | grep -c OPENAI_API_KEY || true"}),
            &ToolCallContext::default(),
        )
        .await
        .expect("command runs");

    unsafe {
        std::env::remove_var("OPENAI_API_KEY");
    }

    assert!(
        !output.contains("sk-must-not-leak"),
        "shell command read a provider key from the environment: {output}"
    );
    assert!(
        output.contains("key=[absent]"),
        "expected the variable to be unset in the child: {output}"
    );
}

#[tokio::test]
async fn command_keeps_the_path_it_needs_to_run() {
    let output = ShellTool::new()
        .execute(
            json!({"command": "test -n \"$PATH\" && echo path-present"}),
            &ToolCallContext::default(),
        )
        .await
        .expect("command runs");
    assert!(output.contains("path-present"), "child lost PATH: {output}");
}

#[tokio::test]
async fn cancellation_drop_terminates_background_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("cancelled-child-survived");
    let marker = marker.to_string_lossy().replace('\'', "'\\''");
    let command = format!("(sleep 1; echo leaked > '{marker}') & wait");

    let task = tokio::spawn(async move {
        ShellTool::new()
            .execute(
                json!({"command": command, "timeout_secs": 10}),
                &ToolCallContext::default(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    task.abort();
    let _ = task.await;

    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(
        !dir.path().join("cancelled-child-survived").exists(),
        "background child survived future cancellation"
    );
}

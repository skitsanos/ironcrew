//! CLI-mode `crew:ask_human()` behavior. Test processes have no TTY on
//! stdin, so these exercise exactly the unattended path the spec defines:
//! immediate timeout → `default` if provided, clean Lua error otherwise.

use ironcrew::cli::commands::cmd_run;

fn write_flow(script: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("crew.lua"), script).unwrap();
    dir
}

#[tokio::test]
async fn non_tty_ask_human_falls_through_to_default() {
    let dir = write_flow(
        r#"
        local crew = Crew.new({
            goal = "cli ask",
            provider = "openai",
            model = "test",
            api_key = "test",
        })
        local answer = crew:ask_human({
            prompt = "Unattended?",
            timeout_s = 5,
            default = "fallback",
        })
        if answer ~= "fallback" then
            error("expected default, got: " .. tostring(answer))
        end
    "#,
    );

    // Must complete quickly (no hang waiting on stdin) and succeed.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        cmd_run(dir.path(), None, false, Vec::new()),
    )
    .await
    .expect("cmd_run hung on non-TTY stdin");
    result.expect("flow should complete via the default value");
}

#[tokio::test]
async fn non_tty_ask_human_without_default_errors_cleanly() {
    let dir = write_flow(
        r#"
        local crew = Crew.new({
            goal = "cli ask",
            provider = "openai",
            model = "test",
            api_key = "test",
        })
        crew:ask_human({ prompt = "Unattended?", timeout_s = 5 })
    "#,
    );

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        cmd_run(dir.path(), None, false, Vec::new()),
    )
    .await
    .expect("cmd_run hung on non-TTY stdin");
    let err = result.expect_err("flow should fail without a default");
    assert!(err.to_string().contains("timed out"), "got: {err}");
}

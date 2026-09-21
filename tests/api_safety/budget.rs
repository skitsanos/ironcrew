use super::*;

#[test]
fn budget_denials_are_terminal_and_visible_even_when_lua_catches_them() {
    let name = "budget::budget_denials_are_terminal_and_visible_even_when_lua_catches_them";
    if std::env::var("IRONCREW_BUDGET_HTTP_CHILD").as_deref() != Ok(name) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("IRONCREW_BUDGET_HTTP_CHILD", name)
            .env("IRONCREW_MAX_RUN_TOKENS", "100")
            .env("OPENAI_API_KEY", "fixture-not-a-secret")
            .env_remove("IRONCREW_RATE_LIMIT_MS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let server = spawn_server(2, 2, Duration::from_secs(10)).await;
            std::fs::write(
                server.root.join("flow-a/crew.lua"),
                r#"
            local crew=Crew.new({goal='budget',provider='openai',model='fixture',api_key='fixture'})
            crew:add_agent(Agent.new({name='one',goal='work'}))
            crew:add_task({name='task',agent='one',description='work'})
            assert(not pcall(function() crew:run() end))
            -- Swallowing the Lua error must not produce a successful HTTP run.
        "#,
            )
            .unwrap();
            let client = reqwest::Client::new();
            let response: serde_json::Value = start_run(&client, &server, "flow-a")
                .await
                .json()
                .await
                .unwrap();
            let id = response["run_id"].as_str().unwrap();
            wait_for_status(&server.store, id, RunStatus::Failed).await;
            let saved = server.store.get_run(id).await.unwrap();
            assert_eq!(
                saved.usage.budget.state,
                ironcrew::usage::budget::BudgetState::Unsupported
            );
            assert_eq!(saved.usage.budget.limit, Some(100));
            assert_eq!(saved.usage.settled.requests(), 0);
            let events = client
                .get(format!("{}/flows/flow-a/events/{id}", server.base))
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(events.contains("unsupported"));
            assert!(events.contains("cannot safely bound"));

            let started = client
                .post(format!(
                    "{}/flows/chat/conversations/budget/start",
                    server.base
                ))
                .json(&serde_json::json!({"agent":"tutor"}))
                .send()
                .await
                .unwrap();
            assert!(started.status().is_success());
            for keyed in [false, true] {
                let mut message = client
                    .post(format!(
                        "{}/flows/chat/conversations/budget/messages",
                        server.base
                    ))
                    .json(&serde_json::json!({"content":"must not dispatch"}));
                if keyed {
                    message = message.header("Idempotency-Key", "budget-message");
                }
                let response = message.send().await.unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
                let body: serde_json::Value = response.json().await.unwrap();
                assert_eq!(body["budget"]["state"], "unsupported");
                assert_eq!(body["budget"]["charged"], "0");
                assert!(body.get("assistant").is_none());
            }

            let cli = std::process::Command::new(env!("CARGO_BIN_EXE_ironcrew"))
                .args(["run", server.root.join("flow-a").to_str().unwrap()])
                .env("IRONCREW_MAX_RUN_TOKENS", "100")
                .output()
                .unwrap();
            assert!(!cli.status.success());
            assert!(String::from_utf8_lossy(&cli.stderr).contains("cannot safely bound"));
            let invalid = std::process::Command::new(env!("CARGO_BIN_EXE_ironcrew"))
                .args(["run", server.root.join("flow-a").to_str().unwrap()])
                .env("IRONCREW_MAX_RUN_TOKENS", "oops")
                .output()
                .unwrap();
            assert!(!invalid.status.success());
            assert!(String::from_utf8_lossy(&invalid.stderr).contains("IRONCREW_MAX_RUN_TOKENS"));
        });
}

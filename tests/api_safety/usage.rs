use super::*;

#[test]
fn http_terminal_usage_includes_post_crew_calls_and_survives_abort() {
    let name = "usage::http_terminal_usage_includes_post_crew_calls_and_survives_abort";
    if std::env::var("IRONCREW_USAGE_HTTP_CHILD").as_deref() != Ok(name) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("IRONCREW_USAGE_HTTP_CHILD", name)
            .env("IRONCREW_ALLOW_PRIVATE_IPS", "true")
            .env("OPENAI_API_KEY", "loopback-only")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
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
        .block_on(check_http_usage());
}

async fn check_http_usage() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let provider = axum::Router::new().route("/chat/completions", axum::routing::post(|| async {
        axum::Json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14,
                "prompt_tokens_details": {"cached_tokens": 3},
                "completion_tokens_details": {"reasoning_tokens": 2}}
        }))
    }));
    let mock = tokio::spawn(async move {
        axum::serve(listener, provider).await.unwrap();
    });
    for abort in [false, true] {
        let server = spawn_server(2, 2, Duration::from_secs(30)).await;
        let park = if abort {
            "crew:ask_human({prompt='park', timeout_s=30})"
        } else {
            ""
        };
        std::fs::write(
            server.root.join("flow-a/crew.lua"),
            format!(
                r#"
            local crew = Crew.new({{goal='usage', provider='openai', model='fixture',
                api_key='loopback-only', base_url='http://{address}', stream=false}})
            crew:add_agent(Agent.new({{name='one', goal='work'}}))
            crew:add_task({{name='task', agent='one', description='work'}})
            assert(crew:run()[1].usage.settled.requests == '1')
            local chat = crew:conversation({{agent='one', stream=false}})
            chat:send('post-crew work')
            {park}
        "#
            ),
        )
        .unwrap();
        let client = reqwest::Client::new();
        let accepted = start_run(&client, &server, "flow-a").await;
        assert!(accepted.status().is_success());
        let accepted: serde_json::Value = accepted.json().await.unwrap();
        let run_id = accepted["run_id"].as_str().unwrap();
        for _ in 0..200 {
            if let Ok(record) = server.store.get_run(run_id).await {
                if record.status == RunStatus::Failed {
                    let runs = server.state.active_runs.read().await;
                    let events = runs.get(run_id).unwrap().eventbus.subscribe_with_replay().0;
                    panic!("unexpected failed fixture: {record:?}; events: {events:?}");
                }
                if record.status == RunStatus::Success
                    || record.status == RunStatus::WaitingForInput
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if abort {
            wait_for_status(&server.store, run_id, RunStatus::WaitingForInput).await;
            assert!(
                client
                    .post(format!("{}/flows/flow-a/abort/{run_id}", server.base))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
        }
        let status = if abort {
            RunStatus::Aborted
        } else {
            RunStatus::Success
        };
        wait_for_status(&server.store, run_id, status).await;
        let record = server.store.get_run(run_id).await.unwrap();
        assert_eq!(
            record.usage.coverage,
            ironcrew::usage::UsageCoverage::Complete
        );
        assert_eq!(record.usage.settled.requests(), 2);
        assert_eq!(record.usage.settled.total_tokens().known(), Some(28));
        assert_eq!(record.usage.settled.reasoning_tokens().known(), Some(4));
        assert_eq!(record.task_results[0].usage.settled.requests(), 1);
        let events = client
            .get(format!("{}/flows/flow-a/events/{run_id}", server.base))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let terminal: serde_json::Value = events
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
            .find(|event| event["event"] == "run_complete")
            .expect("terminal SSE event");
        assert_eq!(
            terminal["data"]["usage"],
            serde_json::to_value(&record.usage).unwrap()
        );
    }
    mock.abort();
    let _ = mock.await;
}

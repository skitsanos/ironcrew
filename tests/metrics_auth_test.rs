//! Process-level contract for authenticated, low-cardinality metrics.

use std::fs::{self, File};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "metrics-test-token-32-visible-bytes";

struct MetricsServer {
    child: Child,
    base_url: String,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    _workspace: tempfile::TempDir,
}

impl MetricsServer {
    async fn start(client: &reqwest::Client) -> Self {
        Self::start_with_retention(client, 1).await
    }

    async fn start_with_retention(client: &reqwest::Client, retention_secs: u64) -> Self {
        let workspace = tempfile::tempdir().expect("create metrics workspace");
        let fast_flow = workspace.path().join("fast");
        fs::create_dir(&fast_flow).expect("create fast terminal flow");
        fs::write(
            fast_flow.join("crew.lua"),
            "error('intentional fast terminal run')\n",
        )
        .expect("write fast terminal flow");
        let skipped_flow = workspace.path().join("skipped");
        fs::create_dir(&skipped_flow).expect("create skipped-task flow");
        fs::write(
            skipped_flow.join("crew.lua"),
            r#"
local crew = Crew.new({ goal = "metrics", provider = "openai", model = "test", api_key = "test" })
crew:add_agent(Agent.new({ name = "offline", goal = "metrics", capabilities = { "testing" } }))
crew:add_task_if("false", {
    name = "skipped",
    agent = "offline",
    description = "provider-free skipped task",
    expected_output = "none",
})
crew:run()
"#,
        )
        .expect("write skipped-task flow");
        let parked_flow = workspace.path().join("parked");
        fs::create_dir(&parked_flow).expect("create parked flow");
        fs::write(
            parked_flow.join("crew.lua"),
            r#"
local crew = Crew.new({ goal = "metrics", provider = "openai", model = "test", api_key = "test" })
crew:ask_human({ prompt = "Hold the metrics stream", timeout_s = 600 })
"#,
        )
        .expect("write parked flow");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve metrics port");
        let port = listener.local_addr().expect("metrics port address").port();
        drop(listener);

        let stdout_path = workspace.path().join("server.stdout.log");
        let stderr_path = workspace.path().join("server.stderr.log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_ironcrew"));
        command
            .env_clear()
            .current_dir(workspace.path())
            .args([
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--flows-dir",
            ])
            .arg(workspace.path())
            .env("IRONCREW_API_TOKEN", TOKEN)
            .env("IRONCREW_API_PRINCIPAL", "metrics-scraper")
            .env("OPENAI_API_KEY", "unused-process-test-key")
            .env("IRONCREW_MAX_ACTIVE_RUNS", "1")
            .env("IRONCREW_MAX_SSE_CONNECTIONS", "1")
            .env(
                "IRONCREW_RUN_SSE_RETENTION_SECS",
                retention_secs.to_string(),
            )
            .env("IRONCREW_MAX_EVENTS", "8")
            .env("IRONCREW_EVENT_REPLAY_MAX_BYTES", "65536")
            .env("IRONCREW_EVENT_MAX_BYTES", "4096")
            .env("IRONCREW_SHUTDOWN_DRAIN_MS", "0")
            .env("RUST_LOG", "ironcrew=warn")
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                File::create(&stdout_path).expect("create metrics stdout"),
            ))
            .stderr(Stdio::from(
                File::create(&stderr_path).expect("create metrics stderr"),
            ));
        for variable in ["PATH", "SystemRoot", "WINDIR", "TMPDIR", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(variable) {
                command.env(variable, value);
            }
        }
        let child = command.spawn().expect("spawn metrics server");
        let mut server = Self {
            child,
            base_url: format!("http://127.0.0.1:{port}"),
            stdout_path,
            stderr_path,
            _workspace: workspace,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Some(status) = server.child.try_wait().expect("inspect metrics server") {
                panic!("metrics server exited with {status}\n{}", server.logs());
            }
            if client
                .get(format!("{}/health/ready", server.base_url))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("metrics server did not become ready\n{}", server.logs());
    }

    fn logs(&self) -> String {
        format!(
            "stdout:\n{}\nstderr:\n{}",
            fs::read_to_string(&self.stdout_path).unwrap_or_default(),
            fs::read_to_string(&self.stderr_path).unwrap_or_default()
        )
    }
}

fn gauge(body: &str, name: &str) -> u64 {
    body.lines()
        .find_map(|line| {
            let (candidate, value) = line.split_once(' ')?;
            (candidate == name).then(|| value.parse().expect("integer gauge"))
        })
        .unwrap_or_else(|| panic!("missing gauge {name}"))
}

fn labeled_counter(body: &str, series: &str) -> u64 {
    body.lines()
        .find_map(|line| {
            let (candidate, value) = line.split_once(' ')?;
            (candidate == series).then(|| value.parse().expect("integer counter"))
        })
        .unwrap_or_else(|| panic!("missing counter {series}"))
}

fn sample_count(body: &str, metric: &str) -> usize {
    body.lines()
        .filter(|line| {
            line.starts_with(metric)
                && line
                    .as_bytes()
                    .get(metric.len())
                    .is_some_and(|byte| matches!(byte, b'{' | b' '))
        })
        .count()
}

async fn scrape(client: &reqwest::Client, server: &MetricsServer) -> String {
    let response = client
        .get(format!("{}/metrics", server.base_url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("scrape metrics");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.text().await.expect("read metrics")
}

async fn wait_for_terminal_bus(client: &reqwest::Client, server: &MetricsServer) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let body = scrape(client, server).await;
        if gauge(&body, "ironcrew_process_active_runs") == 0
            && gauge(&body, "ironcrew_process_eventbus_instances") >= 1
        {
            return body;
        }
        assert!(Instant::now() < deadline, "terminal bus did not appear");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[tokio::test]
async fn metrics_require_authentication_and_keep_the_existing_contract() {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build metrics client");
    let server = MetricsServer::start(&client).await;
    let endpoint = format!("{}/metrics", server.base_url);

    for request in [
        client.get(&endpoint),
        client
            .get(&endpoint)
            .bearer_auth("wrong-metrics-token-32-visible-bytes"),
    ] {
        let response = request.send().await.expect("request protected metrics");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[reqwest::header::CACHE_CONTROL],
            "no-store"
        );
        assert!(!response.text().await.unwrap().contains("ironcrew_"));
    }

    let response = client
        .get(&endpoint)
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("scrape authenticated metrics");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    assert_eq!(
        response.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    let body = response.text().await.expect("read authenticated metrics");
    for existing in [
        "ironcrew_build_info{",
        "ironcrew_process_active_runs 0\n",
        "ironcrew_admission_requests_total{class=\"work\",outcome=\"admitted\"}",
        "ironcrew_idempotency_global_usage{resource=\"records\"}",
    ] {
        assert!(
            body.contains(existing),
            "missing existing metric {existing}"
        );
    }
    assert!(body.contains("ironcrew_store_maintenance_healthy 1\n"));
    assert!(body.contains("ironcrew_process_terminal_persistence_degraded_finalizers 0\n"));
    for metric in [
        "ironcrew_process_active_provider_calls 0\n",
        "ironcrew_process_peak_active_provider_calls 0\n",
        "ironcrew_process_eventbus_instances 0\n",
        "ironcrew_process_eventbus_retained_events 0\n",
        "ironcrew_process_eventbus_retained_bytes 0\n",
        "ironcrew_process_eventbus_retained_events_capacity 0\n",
        "ironcrew_process_eventbus_retained_bytes_capacity 0\n",
    ] {
        assert!(body.contains(metric), "missing resource metric {metric}");
    }
    assert!(body.contains("ironcrew_process_memory_measurement_available "));
    assert!(!body.contains("ironcrew_postgres_pool_open_connections"));
    #[cfg(target_os = "linux")]
    {
        assert!(body.contains("ironcrew_process_memory_measurement_available 1\n"));
        assert!(body.contains("ironcrew_process_resident_memory_bytes "));
        assert!(body.contains("ironcrew_process_peak_resident_memory_bytes "));
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert!(body.contains("ironcrew_process_memory_measurement_available 0\n"));
        assert!(!body.contains("ironcrew_process_resident_memory_bytes "));
        assert!(!body.contains("ironcrew_process_peak_resident_memory_bytes "));
    }
    assert!(!body.contains(TOKEN));
    assert!(!body.contains("metrics-scraper"));

    for (metric, expected_samples) in [
        ("ironcrew_runs_total", 6),
        ("ironcrew_tasks_total", 4),
        ("ironcrew_tool_calls_total", 3),
        ("ironcrew_provider_requests_total", 36),
        ("ironcrew_provider_tokens_total", 12),
        ("ironcrew_sse_connections_total", 6),
        ("ironcrew_lease_losses_total", 2),
        ("ironcrew_reconciliation_cycles_total", 2),
        ("ironcrew_terminal_persistence_total", 15),
        ("ironcrew_store_failures_total", 13),
    ] {
        assert_eq!(
            sample_count(&body, metric),
            expected_samples,
            "unexpected fixed-cardinality sample count for {metric}"
        );
    }
    assert!(
        labeled_counter(
            &body,
            "ironcrew_reconciliation_cycles_total{outcome=\"success\"}"
        ) >= 1,
        "startup reconciliation must be observable"
    );

    let skipped = client
        .post(format!("{}/flows/skipped/run", server.base_url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("start skipped-task run");
    assert_eq!(skipped.status(), reqwest::StatusCode::OK);
    let skipped_metrics = wait_for_terminal_bus(&client, &server).await;
    assert_eq!(
        labeled_counter(
            &skipped_metrics,
            "ironcrew_tasks_total{outcome=\"skipped\"}"
        ),
        1,
    );

    let parked = client
        .post(format!("{}/flows/parked/run", server.base_url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("start parked run");
    assert_eq!(parked.status(), reqwest::StatusCode::OK);
    let parked: serde_json::Value = parked.json().await.expect("decode parked run");
    let parked_run_id = parked["run_id"].as_str().expect("parked run id");
    let events_url = parked["events_url"].as_str().expect("parked events URL");
    let first_stream = client
        .get(format!("{}{}", server.base_url, events_url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("open first SSE stream");
    assert_eq!(first_stream.status(), reqwest::StatusCode::OK);
    let limited = client
        .get(format!("{}{}", server.base_url, events_url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("open over-limit SSE stream");
    assert_eq!(limited.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        limited.headers()[reqwest::header::CACHE_CONTROL],
        "no-store"
    );
    let sse_metrics = scrape(&client, &server).await;
    assert_eq!(
        labeled_counter(
            &sse_metrics,
            "ironcrew_sse_connections_total{scope=\"run_process\",outcome=\"accepted\"}"
        ),
        1,
    );
    assert_eq!(
        labeled_counter(
            &sse_metrics,
            "ironcrew_sse_connections_total{scope=\"run_process\",outcome=\"limited\"}"
        ),
        1,
    );
    let aborted = client
        .post(format!(
            "{}/flows/parked/abort/{}",
            server.base_url, parked_run_id
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .expect("abort parked run");
    assert_eq!(aborted.status(), reqwest::StatusCode::OK);
    drop(first_stream);
}

#[tokio::test]
async fn terminal_eventbus_retention_has_one_owner_and_remains_replayable() {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build metrics client");
    // A long retention makes the old hidden-monitor clone deterministic: it
    // cannot self-heal between requests even on a slow CI worker.
    let server = MetricsServer::start_with_retention(&client, 30).await;

    for run_index in 0..3 {
        let response = client
            .post(format!("{}/flows/fast/run", server.base_url))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "run": run_index }))
            .send()
            .await
            .expect("start fast terminal run");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let started: serde_json::Value = response.json().await.expect("decode run start");
        let terminal = wait_for_terminal_bus(&client, &server).await;
        assert_eq!(
            labeled_counter(&terminal, "ironcrew_runs_total{outcome=\"failed\"}"),
            run_index + 1,
        );
        assert_eq!(
            labeled_counter(
                &terminal,
                "ironcrew_terminal_persistence_total{scope=\"run_record\",outcome=\"success\"}"
            ),
            run_index + 1,
        );
        assert_eq!(
            gauge(&terminal, "ironcrew_process_eventbus_instances"),
            1,
            "a completed monitor must not retain a hidden EventBus clone"
        );
        assert_eq!(
            gauge(
                &terminal,
                "ironcrew_process_eventbus_retained_events_capacity"
            ),
            8
        );
        assert_eq!(
            gauge(
                &terminal,
                "ironcrew_process_eventbus_retained_bytes_capacity"
            ),
            65_536
        );

        if run_index == 0 {
            let events_url = started["events_url"].as_str().expect("events URL");
            let replay = client
                .get(format!("{}{}", server.base_url, events_url))
                .bearer_auth(TOKEN)
                .send()
                .await
                .expect("request late terminal replay");
            assert_eq!(replay.status(), reqwest::StatusCode::OK);
            assert!(
                replay
                    .text()
                    .await
                    .expect("read late replay")
                    .contains("event: run_complete")
            );
            let after_replay = scrape(&client, &server).await;
            assert_eq!(
                labeled_counter(
                    &after_replay,
                    "ironcrew_sse_connections_total{scope=\"run_process\",outcome=\"accepted\"}"
                ),
                1,
            );
        }
    }

    drop(server);

    // Use a separate short-lived process for deregistration evidence instead
    // of waiting through the adversarial 30-second retention window.
    let server = MetricsServer::start(&client).await;
    let response = client
        .post(format!("{}/flows/fast/run", server.base_url))
        .bearer_auth(TOKEN)
        .json(&serde_json::json!({ "cleanup": true }))
        .send()
        .await
        .expect("start cleanup run");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let _ = wait_for_terminal_bus(&client, &server).await;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let body = scrape(&client, &server).await;
        let values = [
            gauge(&body, "ironcrew_process_eventbus_instances"),
            gauge(&body, "ironcrew_process_eventbus_retained_events"),
            gauge(&body, "ironcrew_process_eventbus_retained_bytes"),
            gauge(&body, "ironcrew_process_eventbus_retained_events_capacity"),
            gauge(&body, "ironcrew_process_eventbus_retained_bytes_capacity"),
        ];
        if values == [0; 5] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "terminal EventBus did not deregister"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

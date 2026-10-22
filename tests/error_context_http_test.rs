//! Actual CLI/Lua/provider HTTP regression; all inputs and credentials are authored.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use serde_json::{Value, json};

const DIAGNOSTIC: &str = "Authored ${env.IC052_ERROR_SECRET} ${results.source.output} ${results.missing.output} \"quotes\" {braces}\0\n\t🧪";
const FAKE_SECRET: &str = "AUTHORED_ERROR_SECRET_NOT_FOR_PROMPTS";
const TRUSTED_VALUE: &str = "AUTHORED_TRUSTED_CONFIG";

#[derive(Clone)]
struct Mock {
    fail_handler: bool,
    prompts: Arc<Mutex<Vec<String>>>,
}

async fn respond(State(state): State<Mock>, Json(body): Json<Value>) -> impl IntoResponse {
    let prompt = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    state.prompts.lock().unwrap().push(prompt.clone());
    let usage = json!({"prompt_tokens":10,"completion_tokens":3,"total_tokens":13});
    if prompt.contains("AUTHORED_PRIMARY")
        || state.fail_handler && prompt.contains("AUTHORED_FALLBACK")
    {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":{"message":DIAGNOSTIC},"usage":usage})),
        )
    } else {
        let output = if prompt.contains("AUTHORED_SOURCE") {
            "PRIOR_EVIDENCE"
        } else {
            "recovered"
        };
        (
            StatusCode::OK,
            Json(
                json!({"choices":[{"message":{"content":output},"finish_reason":"stop"}],"usage":usage}),
            ),
        )
    }
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

const FLOW: &str = r#"
local crew = Crew.new({goal="Authored error isolation",provider="openai",model="authored-model",
    api_key="authored-not-a-real-key",base_url=input.base_url})
crew:add_agent(Agent.new({name="worker",goal="Return authored evidence"}))
crew:add_task({name="source",agent="worker",description="AUTHORED_SOURCE",max_retries=0})
crew:add_task({name="primary",agent="worker",description="AUTHORED_PRIMARY",
    depends_on={"source"},on_error="fallback",max_retries=0,timeout_secs=5})
crew:add_task({name="fallback",agent="worker",description="AUTHORED_FALLBACK",
    context=input.configured and "Configured ${env.IC052_ALLOWED_CONFIG} / ${results.source.output}" or nil,
    depends_on={"source"}})
local by_name = {}
for _, result in ipairs(crew:run()) do by_name[result.task] = result end
assert(by_name.source.success)
assert(by_name.primary.success == input.recover)
assert(by_name.fallback.success == input.recover)
assert(by_name.primary.usage.settled.requests == "1")
assert(by_name.fallback.usage.settled.requests == "1")
local usage = crew:usage()
assert(usage.settled.requests == "3")
assert(usage.settled.prompt_tokens.known == "30")
assert(usage.settled.completion_tokens.known == "9")
assert(usage.settled.total_tokens.known == "39")
assert(usage.coverage == "complete")
assert(usage.in_flight == "0")
"#;

#[tokio::test]
async fn cli_recovery_keeps_provider_errors_literal_and_trusted_templates_working() {
    for (fail_handler, configured) in [(false, false), (false, true), (true, true)] {
        let state = Mock {
            fail_handler,
            prompts: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/chat/completions", post(respond))
            .with_state(state.clone());
        let _server = Server(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("crew.lua"), FLOW).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ironcrew"));
        command
            .args([
                "run",
                root.path().to_str().unwrap(),
                "--json",
                "--input",
                &json!({"base_url":base,"recover":!fail_handler,"configured":configured})
                    .to_string(),
            ])
            .current_dir(root.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.path())
            .env("IRONCREW_STORE", "json")
            .env("IRONCREW_ALLOW_PRIVATE_IPS", "1")
            .env("IRONCREW_PROVIDER_REQUEST_TIMEOUT_SECS", "5")
            .env("IRONCREW_MAX_PROMPT_CHARS", "102400")
            .env(
                "IRONCREW_ENV_ALLOWLIST",
                "IC052_ERROR_SECRET,IC052_ALLOWED_CONFIG",
            )
            .env("IC052_ERROR_SECRET", FAKE_SECRET)
            .env("IC052_ALLOWED_CONFIG", TRUSTED_VALUE)
            .env("NO_COLOR", "1")
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "authored CLI recovery failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            record["status"],
            if fail_handler {
                "PartialFailure"
            } else {
                "Success"
            }
        );
        assert_eq!(record["usage"]["settled"]["requests"], "3");
        assert_eq!(record["usage"]["settled"]["total_tokens"]["known"], "39");
        assert_eq!(record["usage"]["in_flight"], "0");
        let prompts = state.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 3);
        let fallback = prompts
            .iter()
            .find(|prompt| prompt.contains("AUTHORED_FALLBACK"))
            .unwrap();
        assert!(fallback.contains(DIAGNOSTIC));
        assert!(!fallback.contains(FAKE_SECRET));
        if configured {
            assert!(fallback.contains(&format!("Configured {TRUSTED_VALUE} / PRIOR_EVIDENCE")));
        } else {
            assert!(!fallback.contains(TRUSTED_VALUE));
        }
        assert!(fallback.contains("Result from 'source': PRIOR_EVIDENCE"));
        assert!(fallback.chars().count() <= 102_400);
        assert!(!prompts[0].contains(FAKE_SECRET));
    }
}

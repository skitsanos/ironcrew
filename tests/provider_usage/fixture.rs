use std::future::Future;
use std::process::Command;

use ironcrew::engine::agent::Agent;
use ironcrew::llm::anthropic::{AnthropicConfig, AnthropicProvider};
use ironcrew::llm::openai::OpenAiProvider;
use ironcrew::llm::openai_responses::{OpenAiResponsesProvider, ResponsesConfig};
use ironcrew::llm::provider::{ChatMessage, ChatRequest, LlmProvider};
use ironcrew::usage::UsageTracker;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Environment policy is set only in a dedicated child process, never mutated
/// under the parent test runner's threads. Each child runs exactly one test.
pub fn isolated(name: &str, check: impl Future<Output = ()>) {
    if std::env::var("IRONCREW_USAGE_TEST_CHILD").as_deref() == Ok(name) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(check);
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("IRONCREW_USAGE_TEST_CHILD", name)
        .env("IRONCREW_ALLOW_PRIVATE_IPS", "true")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("IRONCREW_PROVIDER_REQUEST_TIMEOUT_SECS", "5")
        .env_remove("IRONCREW_RATE_LIMIT_MS")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[derive(Clone, Copy)]
pub enum Provider {
    Chat,
    Responses,
    Anthropic,
}

impl Provider {
    pub fn create(self, base: String) -> Box<dyn LlmProvider> {
        let key = "local-fixture-not-a-secret".to_owned();
        match self {
            Self::Chat => Box::new(OpenAiProvider::new(key, Some(base))),
            Self::Responses => Box::new(OpenAiResponsesProvider::new(
                key,
                Some(base),
                ResponsesConfig::default(),
            )),
            Self::Anthropic => Box::new(AnthropicProvider::new(
                key,
                Some(base),
                AnthropicConfig::default(),
            )),
        }
    }
}

pub fn request(tracker: &UsageTracker) -> ChatRequest {
    let mut request = Agent::default().chat_request(
        "fixture-model".into(),
        vec![ChatMessage::system("fixture"), ChatMessage::user("hello")],
    );
    request.usage_tracker = Some(tracker.clone());
    request
}

pub struct Server {
    pub base: String,
    task: tokio::task::JoinHandle<Value>,
}

impl Server {
    /// A held stream advertises one additional byte, so EOF cannot finish it.
    /// The test must abort the provider future to exercise cancellation.
    pub async fn new(status: u16, content_type: &str, body: String, hold: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let content_type = content_type.to_owned();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut content_length = 0;
            let mut header_bytes = 0;
            loop {
                let mut line = String::new();
                assert!(stream.read_line(&mut line).await.unwrap() > 0);
                header_bytes += line.len();
                assert!(header_bytes < 64 * 1024);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            assert!(content_length < 64 * 1024);
            let mut input = vec![0; content_length];
            stream.read_exact(&mut input).await.unwrap();
            let request: Value = serde_json::from_slice(&input).unwrap();
            let headers = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() + usize::from(hold)
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            if hold {
                std::future::pending::<()>().await;
            }
            request
        });
        Self { base, task }
    }

    pub async fn sent_request(&mut self) -> Value {
        (&mut self.task).await.unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn event(kind: &str, data: Value) -> String {
    format!("event: {kind}\ndata: {data}\n\n")
}

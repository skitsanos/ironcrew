use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironcrew::engine::runtime::Runtime;
use ironcrew::llm::provider::{
    ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
};
use ironcrew::lua::api::{register_agent_constructor, register_crew_constructor};
use ironcrew::lua::sandbox::create_crew_lua;
use ironcrew::usage::{UsageCounts, UsageCoverage, UsageReceipt, UsageTracker};
use ironcrew::utils::error::{IronCrewError, Result};

pub enum Step {
    Reply(&'static str),
    Tool(&'static str),
    Fail,
    Hang(Arc<tokio::sync::Notify>),
}

#[derive(Clone, Default)]
pub struct Recorder {
    steps: Arc<Mutex<VecDeque<Step>>>,
    pub calls: Arc<AtomicU64>,
}

impl Recorder {
    pub fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            ..Default::default()
        }
    }
}

#[async_trait]
impl LlmProvider for Recorder {
    fn records_usage(&self) -> bool {
        true
    }
    fn execution_fingerprint(&self) -> Result<String> {
        Ok(format!("sha256:{}", "1".repeat(64)))
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let tracker = request
            .usage_tracker
            .expect("execution must supply a scope");
        let mut attempt = tracker.start().unwrap();
        self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Step::Reply("done"));
        let receipt = UsageReceipt::from_counts(
            UsageCounts {
                prompt_tokens: Some(10),
                completion_tokens: Some(3),
                total_tokens: Some(13),
                cached_tokens: Some(5),
                cache_write_tokens: Some(0),
                reasoning_tokens: Some(2),
            },
            !matches!(step, Step::Hang(_)),
        );
        attempt.observe(receipt.clone());
        tokio::task::yield_now().await;
        let response = match step {
            Step::Hang(entered) => {
                entered.notify_one();
                std::future::pending::<ChatResponse>().await
            }
            Step::Fail => {
                attempt.finish(receipt).unwrap();
                return Err(IronCrewError::Provider("fixture failure".into()));
            }
            Step::Reply(content) => ChatResponse {
                content: Some(content.into()),
                ..Default::default()
            },
            Step::Tool(name) => ChatResponse {
                tool_calls: vec![ToolCallRequest {
                    id: "call-fixture".into(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: name.into(),
                        arguments: r#"{"prompt":"delegate"}"#.into(),
                    },
                }],
                ..Default::default()
            },
        };
        attempt.finish(receipt).unwrap();
        Ok(response)
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

pub fn vm(root: &Path, runtime: Arc<Runtime>) -> mlua::Lua {
    let lua = create_crew_lua().unwrap();
    lua.set_app_data(runtime.clone());
    lua.set_app_data(Arc::new(root.to_path_buf()));
    register_agent_constructor(&lua).unwrap();
    register_crew_constructor(&lua, runtime, Vec::new(), root.to_path_buf()).unwrap();
    lua
}

pub fn runtime(root: &Path, recorder: Recorder) -> Arc<Runtime> {
    let runtime = Arc::new(Runtime::new(Box::new(recorder), Some(root)));
    runtime.set_self_ref(Arc::downgrade(&runtime));
    runtime
}

pub fn tracker(lua: &mlua::Lua) -> UsageTracker {
    lua.app_data_ref::<UsageTracker>()
        .expect("automatic VM scope")
        .clone()
}

pub fn assert_usage(tracker: &UsageTracker, requests: u64, coverage: UsageCoverage) {
    let snapshot = tracker.snapshot().unwrap();
    assert_eq!(snapshot.in_flight, 0);
    assert_eq!(snapshot.coverage, coverage);
    assert_eq!(snapshot.settled.requests(), requests);
    assert_eq!(snapshot.settled.total_tokens().known(), Some(13 * requests));
    assert_eq!(
        snapshot.settled.reasoning_tokens().known(),
        Some(2 * requests)
    );
}

pub const CREW: &str = r#"
    local crew = Crew.new({ goal = "usage test", provider = "openai", model = "fixture" })
    crew:add_agent(Agent.new({ name = "alice", goal = "work" }))
    crew:add_agent(Agent.new({ name = "bob", goal = "review" }))
"#;

pub const ONE_TASK: &str = r#"
    crew:add_task({ name = "one", agent = "alice", description = "work" })
    local results = crew:run()
    assert(results[1].success, results[1].output)
"#;

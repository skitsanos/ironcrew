//! Integration tests for `crew:ask_human()` — the mid-run human-input
//! primitive. Hermetic: the HTTP-mode bridge is driven directly from the
//! test (no server, no LLM calls — NoopProvider errors loudly if a test
//! accidentally reaches `crew:run()`).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ironcrew::engine::eventbus::{CrewEvent, EventBus};
use ironcrew::engine::input_bridge::{AskHumanContext, BridgeMode, InputBridge};
use ironcrew::engine::runtime::Runtime;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, ToolSchema};
use ironcrew::lua::api::register_crew_constructor;
use ironcrew::lua::sandbox::create_crew_lua;
use ironcrew::utils::error::IronCrewError;

struct NoopProvider;

#[async_trait]
impl LlmProvider for NoopProvider {
    async fn chat(&self, _request: ChatRequest) -> ironcrew::utils::error::Result<ChatResponse> {
        Err(IronCrewError::Provider("NoopProvider: no LLM calls".into()))
    }

    async fn chat_with_tools(
        &self,
        _request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> ironcrew::utils::error::Result<ChatResponse> {
        Err(IronCrewError::Provider("NoopProvider: no LLM calls".into()))
    }
}

/// Minimal Lua VM with a Crew constructor and an HTTP-mode input bridge
/// injected as app data — the same wiring `ironcrew serve` performs.
fn fixture(project_dir: &Path) -> (mlua::Lua, Arc<InputBridge>, EventBus) {
    let lua = create_crew_lua().expect("create_crew_lua");

    let provider = Box::new(NoopProvider);
    let runtime = Runtime::new(provider, Some(project_dir));
    let runtime = Arc::new(runtime);
    runtime.set_self_ref(Arc::downgrade(&runtime));

    lua.set_app_data(runtime.clone());
    lua.set_app_data(Arc::new(project_dir.to_path_buf()));

    register_crew_constructor(&lua, runtime, Vec::new(), project_dir.to_path_buf())
        .expect("register_crew_constructor");

    let bridge = Arc::new(InputBridge::new(BridgeMode::Http));
    lua.set_app_data(AskHumanContext {
        bridge: bridge.clone(),
        run_id: None, // store-status transitions are covered by store tests
        store: None,
        eventbus: None,
    });

    // Inject an EventBus the way the serve handler does, so we can assert
    // on emitted events.
    let eventbus = EventBus::new(256);
    lua.set_app_data(eventbus.clone());

    (lua, bridge, eventbus)
}

const CREW_NEW: &str = r#"
    local crew = Crew.new({
        goal = "ask-human test",
        provider = "openai",
        model = "test",
        api_key = "test",
    })
"#;

#[tokio::test]
async fn ask_human_returns_the_posted_answer() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, bridge, eventbus) = fixture(dir.path());
    let mut events = eventbus.subscribe();

    let script = format!(
        r#"{CREW_NEW}
        return crew:ask_human({{
            prompt = "Proceed with deploy?",
            choices = {{ "yes", "no" }},
            timeout_s = 30,
        }})"#
    );

    let flow = tokio::spawn(async move { lua.load(script).eval_async::<String>().await });

    // Wait for the question to be registered, then answer it like the
    // HTTP endpoint would.
    let question_id = loop {
        let pending = bridge.list();
        if let Some(q) = pending.first() {
            assert_eq!(q.prompt, "Proceed with deploy?");
            assert_eq!(q.choices, vec!["yes".to_string(), "no".to_string()]);
            break q.question_id.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    bridge
        .answer(&question_id, serde_json::json!("yes"))
        .unwrap();

    let answer = flow.await.unwrap().unwrap();
    assert_eq!(answer, "yes");

    // Events: requested (with metadata) then received (answered, no content).
    let mut saw_requested = false;
    let mut saw_received = false;
    while let Ok(ev) = events.try_recv() {
        match &*ev {
            CrewEvent::HumanInputRequested {
                question_id: qid,
                prompt,
                choices,
                timeout_s,
                kind,
            } => {
                assert_eq!(kind, "question");
                assert_eq!(qid, &question_id);
                assert_eq!(prompt, "Proceed with deploy?");
                assert_eq!(choices.len(), 2);
                assert_eq!(*timeout_s, 30);
                saw_requested = true;
            }
            CrewEvent::HumanInputReceived {
                question_id: qid,
                outcome,
            } => {
                assert_eq!(qid, &question_id);
                assert_eq!(outcome, "answered");
                saw_received = true;
            }
            _ => {}
        }
    }
    assert!(saw_requested, "HumanInputRequested not emitted");
    assert!(saw_received, "HumanInputReceived not emitted");
}

#[tokio::test]
async fn ask_human_structured_answer_converts_to_lua() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, bridge, _eventbus) = fixture(dir.path());

    let script = format!(
        r#"{CREW_NEW}
        local a = crew:ask_human({{ prompt = "Thresholds?", timeout_s = 30 }})
        return a.max - a.min"#
    );
    let flow = tokio::spawn(async move { lua.load(script).eval_async::<i64>().await });

    while bridge.pending_count() == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let qid = bridge.list()[0].question_id.clone();
    bridge
        .answer(&qid, serde_json::json!({"min": 10, "max": 42}))
        .unwrap();

    assert_eq!(flow.await.unwrap().unwrap(), 32);
}

#[tokio::test]
async fn ask_human_timeout_returns_default() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _bridge, _eventbus) = fixture(dir.path());

    let script = format!(
        r#"{CREW_NEW}
        return crew:ask_human({{
            prompt = "Anyone?",
            timeout_s = 1,
            default = "hold",
        }})"#
    );
    let answer: String = lua.load(script).eval_async().await.unwrap();
    assert_eq!(answer, "hold");
}

#[tokio::test]
async fn ask_human_timeout_without_default_raises() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _bridge, _eventbus) = fixture(dir.path());

    let script = format!(
        r#"{CREW_NEW}
        return crew:ask_human({{ prompt = "Anyone?", timeout_s = 1 }})"#
    );
    let err = lua
        .load(script)
        .eval_async::<String>()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("timed out"), "got: {err}");
}

#[tokio::test]
async fn ask_human_requires_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let (lua, _bridge, _eventbus) = fixture(dir.path());

    let script = format!(
        r#"{CREW_NEW}
        return crew:ask_human({{ timeout_s = 1 }})"#
    );
    let err = lua
        .load(script)
        .eval_async::<String>()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("prompt"), "got: {err}");
}

#[tokio::test]
async fn ask_human_without_bridge_errors_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let lua = create_crew_lua().expect("create_crew_lua");
    let provider = Box::new(NoopProvider);
    let runtime = Arc::new(Runtime::new(provider, Some(dir.path())));
    runtime.set_self_ref(Arc::downgrade(&runtime));
    lua.set_app_data(runtime.clone());
    register_crew_constructor(&lua, runtime, Vec::new(), dir.path().to_path_buf()).unwrap();
    // Note: no AskHumanContext app data set.

    let script = format!(
        r#"{CREW_NEW}
        return crew:ask_human({{ prompt = "hi", timeout_s = 1 }})"#
    );
    let err = lua
        .load(script)
        .eval_async::<String>()
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("unavailable"), "got: {err}");
}

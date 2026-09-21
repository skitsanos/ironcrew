//! IC-042: a blank final model reply must stop the dialog without recording a
//! turn or consuming the remaining turn budget.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ironcrew::engine::eventbus::{CrewEvent, EventBus};
use ironcrew::engine::runtime::Runtime;
use ironcrew::llm::provider::{
    ChatRequest, ChatResponse, LlmProvider, ToolCallFunction, ToolCallRequest, ToolSchema,
};
use ironcrew::lua::api::{register_agent_constructor, register_crew_constructor};
use ironcrew::lua::sandbox::create_crew_lua;
use ironcrew::utils::error::Result;

struct ReplyProvider {
    reply: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for ReplyProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            content: Some(self.reply.clone()),
            ..Default::default()
        })
    }

    async fn chat_with_tools(
        &self,
        request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        self.chat(request).await
    }
}

fn lua_fixture(project_dir: &Path, provider: Box<dyn LlmProvider>) -> (mlua::Lua, EventBus) {
    let runtime = Arc::new(Runtime::new(provider, Some(project_dir)));
    runtime.set_self_ref(Arc::downgrade(&runtime));

    let lua = create_crew_lua().expect("create_crew_lua");
    let eventbus = EventBus::new(256);
    lua.set_app_data(eventbus.clone());
    register_agent_constructor(&lua).expect("register_agent_constructor");
    register_crew_constructor(&lua, runtime, Vec::new(), project_dir.to_path_buf())
        .expect("register_crew_constructor");

    (lua, eventbus)
}

fn fixture(project_dir: &Path, reply: &str) -> (mlua::Lua, Arc<AtomicUsize>, EventBus) {
    let calls = Arc::new(AtomicUsize::new(0));
    let (lua, eventbus) = lua_fixture(
        project_dir,
        Box::new(ReplyProvider {
            reply: reply.to_owned(),
            calls: calls.clone(),
        }),
    );
    (lua, calls, eventbus)
}

fn assert_empty_completion_events(eventbus: &EventBus) {
    let events = eventbus.subscribe_with_replay().0;
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match event.as_ref() {
            CrewEvent::DialogCompleted {
                total_turns,
                stop_reason,
                ..
            } => Some((*total_turns, stop_reason.as_deref())),
            _ => None,
        })
        .collect();

    assert_eq!(completed, vec![(0, Some("empty_response"))]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.as_ref(), CrewEvent::DialogTurn { .. })),
        "a blank reply must not emit a dialog_turn event"
    );
}

const DIALOG_SETUP: &str = r#"
    local crew = Crew.new({
        goal = "blank-reply contract",
        provider = "openai",
        model = "stub-model",
    })
    crew:add_agent(Agent.new({ name = "alice", goal = "negotiate" }))
    crew:add_agent(Agent.new({ name = "bob", goal = "negotiate" }))
    local dialog = crew:dialog({
        agents = { "alice", "bob" },
        starter = "Begin the negotiation.",
        max_turns = 10,
    })
"#;

#[tokio::test]
async fn empty_reply_stops_automatic_dialog_without_recording_a_turn() {
    let project = tempfile::tempdir().unwrap();
    let (lua, calls, eventbus) = fixture(project.path(), "");
    let table: mlua::Table = lua
        .load(format!(
            r#"{DIALOG_SETUP}
            local transcript = dialog:run()
            return {{
                turns = #transcript,
                reason = dialog:stop_reason(),
                stopped = dialog:stopped(),
            }}
            "#
        ))
        .eval_async()
        .await
        .expect("dialog script runs");

    assert_eq!(table.get::<usize>("turns").unwrap(), 0);
    assert_eq!(
        table.get::<Option<String>>("reason").unwrap().as_deref(),
        Some("empty_response")
    );
    assert!(table.get::<bool>("stopped").unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_empty_completion_events(&eventbus);
}

#[tokio::test]
async fn whitespace_reply_stops_forced_dialog_and_blocks_another_call() {
    let project = tempfile::tempdir().unwrap();
    let (lua, calls, eventbus) = fixture(project.path(), " \n\t ");
    let table: mlua::Table = lua
        .load(format!(
            r#"{DIALOG_SETUP}
            local first = dialog:next_turn_from("alice")
            local second = dialog:next_turn_from("bob")
            return {{
                first_nil = first == nil,
                second_nil = second == nil,
                turns = dialog:turn_count(),
                reason = dialog:stop_reason(),
                stopped = dialog:stopped(),
            }}
            "#
        ))
        .eval_async()
        .await
        .expect("manual dialog script runs");

    assert!(table.get::<bool>("first_nil").unwrap());
    assert!(table.get::<bool>("second_nil").unwrap());
    assert_eq!(table.get::<usize>("turns").unwrap(), 0);
    assert_eq!(
        table.get::<Option<String>>("reason").unwrap().as_deref(),
        Some("empty_response")
    );
    assert!(table.get::<bool>("stopped").unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_empty_completion_events(&eventbus);
}

struct ToolThenBlankProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmProvider for ToolThenBlankProvider {
    async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
        panic!("the agent has a tool, so chat_with_tools must be used")
    }

    async fn chat_with_tools(
        &self,
        _request: ChatRequest,
        _tools: &[ToolSchema],
    ) -> Result<ChatResponse> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(ChatResponse {
                tool_calls: vec![ToolCallRequest {
                    id: "render-1".into(),
                    call_type: "function".into(),
                    function: ToolCallFunction {
                        name: "template_render".into(),
                        arguments: r#"{"template":"{{ value }}","data":{"value":"done"}}"#.into(),
                    },
                }],
                ..Default::default()
            }),
            1 => Ok(ChatResponse {
                content: Some("  ".into()),
                ..Default::default()
            }),
            _ => panic!("the dialog called the provider after the blank final reply"),
        }
    }
}

#[tokio::test]
async fn blank_final_reply_after_a_tool_round_stops_without_a_turn() {
    let project = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (lua, eventbus) = lua_fixture(
        project.path(),
        Box::new(ToolThenBlankProvider {
            calls: calls.clone(),
        }),
    );
    let table: mlua::Table = lua
        .load(
            r#"
            local crew = Crew.new({
                goal = "tool-assisted blank-reply contract",
                provider = "openai",
                model = "stub-model",
            })
            crew:add_agent(Agent.new({
                name = "alice",
                goal = "render",
                tools = { "template_render" },
            }))
            crew:add_agent(Agent.new({ name = "bob", goal = "review" }))
            local dialog = crew:dialog({
                agents = { "alice", "bob" },
                starter = "Render the value.",
                max_turns = 10,
            })
            local transcript = dialog:run()
            return {
                turns = #transcript,
                reason = dialog:stop_reason(),
                stopped = dialog:stopped(),
            }
            "#,
        )
        .eval_async()
        .await
        .expect("tool-assisted dialog script runs");

    assert_eq!(table.get::<usize>("turns").unwrap(), 0);
    assert_eq!(
        table.get::<Option<String>>("reason").unwrap().as_deref(),
        Some("empty_response")
    );
    assert!(table.get::<bool>("stopped").unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_empty_completion_events(&eventbus);
}

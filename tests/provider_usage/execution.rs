use super::fixture::{Provider, Server, isolated};
use ironcrew::engine::runtime::Runtime;
use ironcrew::lua::api::{register_agent_constructor, register_crew_constructor};
use ironcrew::lua::sandbox::create_crew_lua;
use ironcrew::usage::{UsageCoverage, UsageTracker};
use serde_json::json;
use std::sync::Arc;

#[test]
fn builtin_transport_receipts_reach_automatic_lua_execution_scope() {
    isolated(
        "execution::builtin_transport_receipts_reach_automatic_lua_execution_scope",
        async {
            for (kind, response) in [
                (
                    Provider::Chat,
                    json!({"choices":[{"message":{"content":"done"}}],
                "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}),
                ),
                (
                    Provider::Responses,
                    json!({"status":"completed","output":[{"type":"message",
                "content":[{"type":"output_text","text":"done"}]}],
                "usage":{"input_tokens":10,"output_tokens":3,"total_tokens":13}}),
                ),
                (
                    Provider::Anthropic,
                    json!({"content":[{"type":"text","text":"done"}],
                "usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}),
                ),
            ] {
                let root = tempfile::tempdir().unwrap();
                let server =
                    Server::new(200, "application/json", response.to_string(), false).await;
                let runtime = Arc::new(Runtime::new(
                    kind.create(server.base.clone()),
                    Some(root.path()),
                ));
                runtime.set_self_ref(Arc::downgrade(&runtime));
                let lua = create_crew_lua().unwrap();
                register_agent_constructor(&lua).unwrap();
                register_crew_constructor(&lua, runtime.clone(), vec![], root.path().to_path_buf())
                    .unwrap();
                lua.load(r#"
                local crew = Crew.new({ goal = "fixture", provider = "openai", model = "fixture-model" })
                crew:add_agent(Agent.new({ name = "alice", goal = "work" }))
                crew:add_task({ name = "one", agent = "alice", description = "work" })
                local results = crew:run()
                assert(results[1].success, results[1].output)
            "#).exec_async().await.unwrap();
                let usage = lua
                    .app_data_ref::<UsageTracker>()
                    .unwrap()
                    .snapshot()
                    .unwrap();
                assert_eq!(usage.settled.requests(), 1);
                assert_eq!(usage.settled.total_tokens().known(), Some(13));
                assert_eq!(usage.coverage, UsageCoverage::Complete);
                assert!(runtime.provider.usage_tracker().is_none());
            }
        },
    );
}

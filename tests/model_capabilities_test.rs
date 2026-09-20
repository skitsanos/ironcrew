//! Offline capability regressions: empty credentials prevent accidental API calls.
use ironcrew::engine::agent::Agent;
use ironcrew::llm::openai::OpenAiProvider;
use ironcrew::llm::openai_responses::{OpenAiResponsesProvider, ResponsesConfig};
use ironcrew::llm::provider::LlmProvider;

fn lua_fixture() -> (tempfile::TempDir, mlua::Lua) {
    use ironcrew::engine::runtime::Runtime;
    use ironcrew::lua::api::{register_agent_constructor, register_crew_constructor};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Runtime::new(
        Box::new(OpenAiProvider::new(String::new(), None)),
        Some(dir.path()),
    ));
    runtime.set_self_ref(Arc::downgrade(&runtime));
    let lua = ironcrew::lua::sandbox::create_crew_lua().unwrap();
    register_agent_constructor(&lua).unwrap();
    register_crew_constructor(&lua, runtime, vec![], dir.path().to_path_buf()).unwrap();
    (dir, lua)
}

#[tokio::test]
async fn lua_agent_construction_validates_actual_provider_and_model_override() {
    for (options, valid) in [
        ("reasoning_effort = 'low'", true),
        ("reasoning_effort = 'minimal'", false),
        ("temperature = 0.7", false),
        ("reasoning_effort = 'low', tools = {'http' }", false),
        ("reasoning_effort = 'none', tools = {'http' }", true),
        (
            "model = 'gpt-5.6-terra', reasoning_effort = 'low', tools = {'http'}",
            true,
        ),
        ("model = 'custom-model', temperature = 0.7", true),
    ] {
        let (_dir, lua) = lua_fixture();
        let script = format!(
            "local c = Crew.new({{ goal = 'test' }})\nc:add_agent(Agent.new({{ name = 'worker', goal = 'work', {options} }}))"
        );
        let result = lua.load(&script).exec_async().await;
        assert_eq!(result.is_ok(), valid, "{options}: {result:?}");
    }
}

#[tokio::test]
async fn lua_responses_crew_effort_is_model_aware_without_sending_a_request() {
    for (effort, valid) in [("low", true), ("none", true), ("minimal", false)] {
        let (_dir, lua) = lua_fixture();
        let result = lua.load(format!("Crew.new({{ goal = 'test', provider = 'openai-responses', api_key = 'offline-test-key', reasoning_effort = '{effort}' }})")).exec_async().await;
        assert_eq!(result.is_ok(), valid, "{effort}: {result:?}");
    }
}

#[tokio::test]
async fn lua_custom_endpoint_does_not_apply_official_luna_restrictions() {
    let (_dir, lua) = lua_fixture();
    lua.load("local c = Crew.new({goal = 'test', model = 'gpt-5.6-luna', base_url = 'https://custom.example/v1', api_key = 'offline-test-key'})\nc:add_agent(Agent.new({name = 'worker', goal = 'work', reasoning_effort = 'low', temperature = 0.7, tools = {'http'}}))")
        .exec_async().await.unwrap();
}

#[tokio::test]
async fn luna_rejects_minimal_before_credentials_or_network() {
    let agent = Agent {
        reasoning_effort: Some("minimal".into()),
        ..Default::default()
    };
    let providers: Vec<Box<dyn LlmProvider>> = vec![
        Box::new(OpenAiProvider::new(String::new(), None)),
        Box::new(OpenAiResponsesProvider::new(
            String::new(),
            None,
            ResponsesConfig::default(),
        )),
    ];
    for provider in providers {
        let error = provider
            .chat(agent.chat_request("gpt-5.6-luna".into(), vec![]))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("reasoning_effort"), "{error}");
    }
}

#[tokio::test]
async fn luna_rejects_nondefault_temperature_before_credentials_or_network() {
    let provider = OpenAiProvider::new(String::new(), None);
    let agent = Agent {
        temperature: Some(0.7),
        ..Default::default()
    };
    let error = provider
        .chat(agent.chat_request("gpt-5.6-luna".into(), vec![]))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("temperature"), "{error}");
}

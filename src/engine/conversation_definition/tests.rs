use super::*;

const TOOLS_FINGERPRINT: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";
const PROVIDER_FINGERPRINT: &str =
    "sha256:3333333333333333333333333333333333333333333333333333333333333333";
const OTHER_TOOLS_FINGERPRINT: &str =
    "sha256:4444444444444444444444444444444444444444444444444444444444444444";
const OTHER_PROVIDER_FINGERPRINT: &str =
    "sha256:5555555555555555555555555555555555555555555555555555555555555555";

fn agent() -> Agent {
    Agent {
        name: "writer".into(),
        goal: "Write a concise answer".into(),
        tools: vec!["web".into()],
        model: Some("agent-model".into()),
        ..Agent::default()
    }
}

fn definition<'a>(source: &'a str, agent: &'a Agent) -> ConversationDefinition<'a> {
    ConversationDefinition {
        source_fingerprint: source,
        agent,
        resolved_model: "resolved-model",
        effective_system_prompt: "Be accurate",
        max_history: 20,
        history_max_bytes: 65_536,
        max_tool_rounds: 10,
        resolved_tools_fingerprint: TOOLS_FINGERPRINT,
        provider_execution_fingerprint: PROVIDER_FINGERPRINT,
        app_db: None,
    }
}

#[test]
fn every_definition_input_changes_the_fingerprint() {
    let source = format!("sha256:{}", "0".repeat(64));
    let base_agent = agent();
    let base = definition(&source, &base_agent);
    let expected = conversation_definition_fingerprint(&base).unwrap();
    let changed_source = format!("sha256:{}", "1".repeat(64));
    let mut changed_agent = base_agent.clone();
    changed_agent.goal.push('!');
    macro_rules! differs {
        ($($field:ident: $value:expr),+ $(,)?) => {$({
            let changed = ConversationDefinition { $field: $value, ..base };
            assert_ne!(expected, conversation_definition_fingerprint(&changed).unwrap());
        })+};
    }
    differs!(
        source_fingerprint: &changed_source, agent: &changed_agent,
        resolved_model: "other", effective_system_prompt: "Other",
        max_history: 21, history_max_bytes: 65_537, max_tool_rounds: 11,
        resolved_tools_fingerprint: OTHER_TOOLS_FINGERPRINT,
        provider_execution_fingerprint: OTHER_PROVIDER_FINGERPRINT,
    );
}

#[test]
fn app_db_definition_changes_the_fingerprint_only_when_present() {
    let source = format!("sha256:{}", "0".repeat(64));
    let base_agent = agent();
    let base = definition(&source, &base_agent);
    let without = conversation_definition_fingerprint(&base).unwrap();
    let value = serde_json::json!({"policy": {"max_rows": 500}, "operations": []});
    let with = conversation_definition_fingerprint(&ConversationDefinition {
        app_db: Some(&value),
        ..base
    })
    .unwrap();
    assert_ne!(without, with);
    let changed = serde_json::json!({"policy": {"max_rows": 100}, "operations": []});
    let with_changed = conversation_definition_fingerprint(&ConversationDefinition {
        app_db: Some(&changed),
        ..base
    })
    .unwrap();
    assert_ne!(with, with_changed);
}

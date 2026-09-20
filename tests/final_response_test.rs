//! IC-043: blank model finals are failures, never successful empty results.
#[path = "final_response/conversation.rs"]
mod conversation;
#[path = "final_response/support.rs"]
mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ironcrew::engine::collaborative::execute_collaborative_task;
use ironcrew::engine::crew::{Crew, ProviderConfig, run_crew};
use ironcrew::engine::eventbus::{CrewEvent, EventBus};
use ironcrew::engine::executor::execute_task_standalone;
use ironcrew::engine::memory::MemoryStore;
use ironcrew::engine::task::Task;
use ironcrew::llm::provider::{ChatMessage, LlmProvider};
use ironcrew::lua::agent_turn::run_single_agent_turn;
use ironcrew::tools::ToolCallContext;
use support::{ScriptedProvider, agent, reply, tool_registry, tool_reply};

#[tokio::test]
async fn task_executor_rejects_missing_empty_and_whitespace_finals() {
    for stream in [false, true] {
        for content in [None, Some(""), Some(" \n\t\u{2003}")] {
            let provider = ScriptedProvider::new(vec![reply(content)]);
            let (registry, _) = tool_registry();
            let task = Task {
                name: "answer".into(),
                description: "Answer".into(),
                ..Default::default()
            };
            let error = execute_task_standalone(
                &task,
                &agent(false),
                provider.as_ref(),
                &registry,
                &HashMap::new(),
                "mock",
                2,
                "",
                "",
                stream,
            )
            .await
            .expect_err("a blank final must fail");
            assert!(error.to_string().contains("Empty response"));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn nonblank_tool_assisted_final_keeps_whitespace_and_reasoning() {
    let provider = ScriptedProvider::new(vec![tool_reply(), reply(Some("  answer\n"))]);
    let (registry, effects) = tool_registry();
    let (output, reasoning, _) = execute_task_standalone(
        &Task::default(),
        &agent(true),
        provider.as_ref(),
        &registry,
        &HashMap::new(),
        "mock",
        2,
        "",
        "",
        false,
    )
    .await
    .unwrap();
    assert_eq!(output, "  answer\n");
    assert_eq!(
        reasoning.as_deref(),
        Some("A summary is not a final answer")
    );
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}

fn crew() -> Crew {
    let mut crew = Crew::new(
        "blank final".into(),
        ProviderConfig {
            provider: "openai".into(),
            model: "mock".into(),
            base_url: None,
            api_key: None,
        },
        MemoryStore::ephemeral(),
    );
    crew.add_agent(agent(true)).unwrap();
    crew.add_task(Task {
        name: "source".into(),
        description: "Use a tool then answer".into(),
        agent: Some("worker".into()),
        max_retries: Some(3),
        retry_backoff_secs: Some(0.001),
        ..Default::default()
    })
    .unwrap();
    crew.add_task(Task {
        name: "dependent".into(),
        description: "Consume source".into(),
        depends_on: vec!["source".into()],
        ..Default::default()
    })
    .unwrap();
    crew
}

#[tokio::test]
async fn blank_task_does_not_retry_tools_or_admit_dependents() {
    for content in [None, Some(""), Some(" \t ")] {
        let crew = crew();
        let provider = ScriptedProvider::new(vec![tool_reply(), reply(content)]);
        let (registry, effects) = tool_registry();
        let results = run_crew(&crew, provider.clone(), &registry).await.unwrap();
        let source = results
            .iter()
            .find(|result| result.task == "source")
            .unwrap();
        assert!(!source.success, "blank final was published as a success");
        assert!(source.output.contains("Empty response"));
        let dependent = results
            .iter()
            .find(|result| result.task == "dependent")
            .unwrap();
        assert!(dependent.output.contains("Skipped"));
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            2,
            "must not retry the task"
        );
        assert_eq!(
            effects.load(Ordering::SeqCst),
            1,
            "completed effects must not replay"
        );
        let events = crew.eventbus.subscribe_with_replay().0;
        assert!(events.iter().any(
            |event| matches!(event.as_ref(), CrewEvent::TaskFailed { task, .. } if task == "source")
        ));
        assert!(!events.iter().any(|event| matches!(event.as_ref(), CrewEvent::TaskCompleted { task, .. } if task == "source")));
    }
}

#[tokio::test]
async fn explicit_error_handler_can_recover_without_replaying_the_failed_task() {
    let mut crew = crew();
    crew.tasks[0].on_error = Some("fallback".into());
    crew.add_task(Task {
        name: "fallback".into(),
        description: "Recover explicitly".into(),
        ..Default::default()
    })
    .unwrap();
    let provider = ScriptedProvider::new(vec![
        tool_reply(),
        reply(Some("")),
        reply(Some("fallback answer")),
        reply(Some("dependent answer")),
    ]);
    let (registry, effects) = tool_registry();
    let results = run_crew(&crew, provider.clone(), &registry).await.unwrap();
    let source = results
        .iter()
        .find(|result| result.task == "source")
        .unwrap();
    assert!(source.success);
    assert_eq!(source.output, "Recovered via 'fallback': fallback answer");
    let dependent = results
        .iter()
        .find(|result| result.task == "dependent")
        .unwrap();
    assert!(dependent.success);
    assert_eq!(dependent.output, "dependent answer");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blank_agent_turn_rolls_back_history_even_after_tools() {
    for with_tool in [false, true] {
        for content in [None, Some(""), Some(" \t\n")] {
            let mut replies = vec![];
            if with_tool {
                replies.push(tool_reply());
            }
            replies.push(reply(content));
            let provider = ScriptedProvider::new(replies);
            let (registry, effects) = tool_registry();
            let ctx = ToolCallContext {
                tool_registry: Some(registry),
                ..Default::default()
            };
            let mut history = vec![
                ChatMessage::system("system"),
                ChatMessage::user("prior"),
                ChatMessage::assistant(Some("prior answer".into()), None),
            ];
            let before = serde_json::to_value(&history).unwrap();
            history.push(ChatMessage::user("new"));
            let erased: Arc<dyn LlmProvider> = provider.clone();
            let error = run_single_agent_turn(
                &agent(with_tool),
                &erased,
                "mock",
                2,
                Some(10),
                &mut history,
                &ctx,
            )
            .await
            .expect_err("blank turn must fail");
            assert!(error.to_string().contains("Empty response"));
            assert_eq!(serde_json::to_value(history).unwrap(), before);
            assert_eq!(
                provider.calls.load(Ordering::SeqCst),
                1 + usize::from(with_tool)
            );
            assert_eq!(effects.load(Ordering::SeqCst), usize::from(with_tool));
        }
    }
}

#[tokio::test]
async fn collaboration_rejects_blank_participant_and_synthesis_replies() {
    for valid_turns in [0, 2] {
        let mut replies = vec![reply(Some("valid contribution")); valid_turns];
        replies.push(reply(Some(" \n ")));
        let provider = ScriptedProvider::new(replies);
        let first = agent(false);
        let mut second = agent(false);
        second.name = "reviewer".into();
        let bus = EventBus::new(16);
        let error = execute_collaborative_task(
            &[&first, &second],
            "discussion",
            "Discuss",
            1,
            provider.clone(),
            &HashMap::new(),
            "",
            "mock",
            "mock",
            &bus,
        )
        .await
        .expect_err("a blank collaboration reply must fail");
        assert!(error.to_string().contains("Empty response"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), valid_turns + 1);
        assert_eq!(
            bus.subscribe_with_replay()
                .0
                .iter()
                .filter(|event| matches!(event.as_ref(), CrewEvent::CollaborationTurn { .. }))
                .count(),
            valid_turns
        );
    }
}

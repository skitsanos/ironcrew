use super::fixture::*;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::eventbus::EventBus;
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::store::StateStore;
use ironcrew::llm::scope::with_usage_tracker;
use ironcrew::lua::conversation::LuaConversationInner;
use ironcrew::lua::dialog::AgentDialog;
use ironcrew::tools::{ToolCallContext, registry::ToolRegistry};
use ironcrew::usage::UsageTracker;
use std::sync::Arc;

fn agent(name: &str) -> Agent {
    Agent {
        name: name.into(),
        goal: "work".into(),
        ..Default::default()
    }
}

async fn conversation(
    store: Arc<dyn StateStore>,
    scope: UsageTracker,
    recorder: Recorder,
) -> LuaConversationInner {
    LuaConversationInner::new_or_resume(
        agent("alice"),
        with_usage_tracker(Arc::new(recorder), scope),
        ToolRegistry::new(),
        "fixture".into(),
        "system".into(),
        Some(10),
        1024 * 1024,
        false,
        2,
        EventBus::new(16),
        Some("session".into()),
        Some(store),
        "usage".into(),
        Some("usage".into()),
        true,
        std::path::PathBuf::from("."),
        reqwest::Client::new(),
        format!("sha256:{}", "1".repeat(64)),
        format!("sha256:{}", "2".repeat(64)),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn conversation_resume_and_explicit_callers_do_not_recharge_history() {
    let root = tempfile::tempdir().unwrap();
    let store: Arc<dyn StateStore> = Arc::new(JsonFileStore::new(root.path().to_owned()).unwrap());
    let first_scope = UsageTracker::default();
    let first = conversation(
        store.clone(),
        first_scope.clone(),
        Recorder::new(vec![Step::Fail]),
    )
    .await;
    assert!(first.run_turn("failure", None).await.is_err());
    assert_eq!(first.message_count().await, 1);
    first.run_turn("success", None).await.unwrap();
    assert_eq!(first.usage_snapshot().unwrap().settled.requests(), 2);
    assert_eq!(
        store
            .get_conversation(Some("usage"), "session")
            .await
            .unwrap()
            .unwrap()
            .usage,
        first.usage_snapshot().unwrap()
    );
    drop(first);
    let second_scope = UsageTracker::default();
    let resumed = conversation(store.clone(), second_scope.clone(), Recorder::default()).await;
    assert_eq!(resumed.usage_snapshot().unwrap().settled.requests(), 2);
    assert_eq!(second_scope.snapshot().unwrap().settled.requests(), 0);
    let explicit = UsageTracker::default();
    resumed
        .run_turn_with_ctx(
            "explicit",
            None,
            &ToolCallContext {
                usage_tracker: Some(explicit.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(explicit.snapshot().unwrap().settled.requests(), 1);
    assert_eq!(second_scope.snapshot().unwrap().settled.requests(), 0);
    resumed.run_turn("bound", None).await.unwrap();
    assert_eq!(second_scope.snapshot().unwrap().settled.requests(), 1);
    assert_eq!(resumed.usage_snapshot().unwrap().settled.requests(), 4);
    assert_eq!(
        resumed
            .usage_snapshot()
            .unwrap()
            .settled
            .total_tokens()
            .known(),
        Some(52)
    );
    assert_eq!(first_scope.snapshot().unwrap().settled.requests(), 2);
    resumed.reset_history().await;
    resumed.persist().await.unwrap();
    let saved = store
        .get_conversation(Some("usage"), "session")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.messages.len(), 1);
    assert_eq!(saved.usage, resumed.usage_snapshot().unwrap());
}

async fn dialog(store: Arc<dyn StateStore>, scope: UsageTracker) -> AgentDialog {
    AgentDialog::new_or_resume(
        vec![agent("alice"), agent("bob")],
        with_usage_tracker(Arc::new(Recorder::default()), scope),
        ToolRegistry::new(),
        "fixture".into(),
        "talk".into(),
        3,
        Some(10),
        false,
        2,
        0,
        EventBus::new(16),
        None,
        None,
        Some("dialog".into()),
        Some(store),
        "usage".into(),
        Some("usage".into()),
        true,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn dialog_resume_counts_only_new_turns_in_new_run() {
    let root = tempfile::tempdir().unwrap();
    let store: Arc<dyn StateStore> = Arc::new(JsonFileStore::new(root.path().to_owned()).unwrap());
    let lua = ironcrew::lua::sandbox::create_crew_lua().unwrap();
    let first_scope = UsageTracker::default();
    let first = dialog(store.clone(), first_scope.clone()).await;
    first.run_one_turn(&lua).await.unwrap();
    assert_eq!(first.usage_snapshot().unwrap().settled.requests(), 1);
    drop(first);
    let next_scope = UsageTracker::default();
    let resumed = dialog(store.clone(), next_scope.clone()).await;
    assert_eq!(resumed.usage_snapshot().unwrap().settled.requests(), 1);
    assert_eq!(next_scope.snapshot().unwrap().settled.requests(), 0);
    resumed.run_all(&lua).await.unwrap();
    assert_eq!(resumed.usage_snapshot().unwrap().settled.requests(), 3);
    assert_eq!(next_scope.snapshot().unwrap().settled.requests(), 2);
    assert_eq!(first_scope.snapshot().unwrap().settled.requests(), 1);
    assert_eq!(
        store
            .get_dialog_state(Some("usage"), "dialog")
            .await
            .unwrap()
            .unwrap()
            .usage,
        resumed.usage_snapshot().unwrap()
    );
}

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use ironcrew::engine::eventbus::{CrewEvent, EventBus};
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::DEFAULT_CHAT_HISTORY_MAX_BYTES;
use ironcrew::lua::conversation::LuaConversationInner;
use ironcrew::tools::registry::ToolRegistry;

use super::support::{ScriptedProvider, agent, reply};

async fn check_rollback(
    root: &Path,
    store: Arc<dyn StateStore>,
    stream: bool,
    blank: Option<&str>,
) {
    let provider = ScriptedProvider::new(vec![
        reply(Some("first")),
        reply(blank),
        reply(Some("recovered")),
    ]);
    let bus = EventBus::new(64);
    let conversation = LuaConversationInner::new_or_resume(
        agent(false),
        provider.clone(),
        ToolRegistry::new(),
        "mock".into(),
        "system".into(),
        Some(10),
        DEFAULT_CHAT_HISTORY_MAX_BYTES,
        stream,
        2,
        bus.clone(),
        Some("blank-final".into()),
        Some(store.clone()),
        "test".into(),
        Some("test".into()),
        true,
        root.to_path_buf(),
        reqwest::Client::new(),
        format!("sha256:{}", "1".repeat(64)),
        format!("sha256:{}", "2".repeat(64)),
    )
    .await
    .unwrap();
    conversation.run_turn("initial", None).await.unwrap();
    let before = serde_json::to_value(conversation.messages_snapshot().await).unwrap();
    let persisted = serde_json::to_value(
        store
            .get_conversation(Some("test"), "blank-final")
            .await
            .unwrap(),
    )
    .unwrap();
    let error = conversation
        .run_turn("do not commit me", None)
        .await
        .expect_err("blank conversation turn must fail");
    assert!(error.to_string().contains("Empty response"));
    assert_eq!(conversation.revision().await, 1);
    assert_eq!(
        serde_json::to_value(conversation.messages_snapshot().await).unwrap(),
        before
    );
    assert_eq!(
        serde_json::to_value(
            store
                .get_conversation(Some("test"), "blank-final")
                .await
                .unwrap()
        )
        .unwrap(),
        persisted
    );
    assert_eq!(
        bus.subscribe_with_replay()
            .0
            .iter()
            .filter(|event| matches!(event.as_ref(), CrewEvent::ConversationTurn { .. }))
            .count(),
        1
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

    let (content, _) = conversation.run_turn("try a new turn", None).await.unwrap();
    assert_eq!(content, "recovered");
    assert_eq!(conversation.revision().await, 2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn blank_conversation_finals_leave_json_and_sqlite_history_unchanged() {
    for stream in [false, true] {
        for blank in [None, Some(""), Some(" \n\t\u{2003}")] {
            let root = tempfile::tempdir().unwrap();
            let stores: [Arc<dyn StateStore>; 2] = [
                Arc::new(JsonFileStore::new(root.path().join("json")).unwrap()),
                Arc::new(SqliteStore::new(root.path().join("state.db")).unwrap()),
            ];
            for store in stores {
                check_rollback(root.path(), store, stream, blank).await;
            }
        }
    }
}

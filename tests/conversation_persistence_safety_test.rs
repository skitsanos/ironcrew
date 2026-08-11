use std::sync::Arc;

use async_trait::async_trait;
use ironcrew::engine::agent::Agent;
use ironcrew::engine::eventbus::EventBus;
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::{ChatRequest, ChatResponse, LlmProvider, ToolSchema};
use ironcrew::lua::conversation::LuaConversationInner;
use ironcrew::tools::registry::ToolRegistry;
use ironcrew::utils::error::Result;

struct EchoProvider;

#[async_trait]
impl LlmProvider for EchoProvider {
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse> {
        let content = request
            .messages
            .last()
            .and_then(|message| message.content.as_deref())
            .unwrap_or_default();
        Ok(ChatResponse {
            content: Some(format!("echo:{content}")),
            reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            raw_blocks: None,
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

#[tokio::test]
async fn failed_autosave_does_not_publish_or_duplicate_a_conversation_turn() {
    let temp = tempfile::tempdir().unwrap();
    let store_root = temp.path().join("state");
    let store: Arc<dyn StateStore> = Arc::new(JsonFileStore::new(store_root.clone()).unwrap());
    let conversation = LuaConversationInner::new_or_resume(
        Agent {
            name: "echo".into(),
            goal: "echo test messages".into(),
            ..Default::default()
        },
        Arc::new(EchoProvider),
        ToolRegistry::new(),
        "test-model".into(),
        "system".into(),
        Some(10),
        ironcrew::llm::provider::DEFAULT_CHAT_HISTORY_MAX_BYTES,
        false,
        2,
        EventBus::new(16),
        Some("durable-turn".into()),
        Some(store.clone()),
        "test flow".into(),
        Some("test-flow".into()),
        true,
        temp.path().to_path_buf(),
        reqwest::Client::new(),
        format!("sha256:{}", "1".repeat(64)),
        format!("sha256:{}", "2".repeat(64)),
    )
    .await
    .unwrap();

    let before = serde_json::to_value(conversation.messages_snapshot().await).unwrap();
    let conversations_dir = store_root.join("conversations");
    std::fs::remove_dir_all(&conversations_dir).unwrap();
    std::fs::write(&conversations_dir, b"force persistence failure").unwrap();

    assert!(conversation.run_turn("hello", None).await.is_err());
    assert_eq!(
        serde_json::to_value(conversation.messages_snapshot().await).unwrap(),
        before,
        "a failed durable save must leave the published transcript untouched"
    );
    assert_eq!(conversation.revision().await, 0);

    std::fs::remove_file(&conversations_dir).unwrap();
    std::fs::create_dir(&conversations_dir).unwrap();
    let (reply, _) = conversation.run_turn("hello", None).await.unwrap();
    assert_eq!(reply, "echo:hello");

    let history = conversation.messages_snapshot().await;
    assert_eq!(
        history
            .iter()
            .filter(|message| message.role == "user")
            .count(),
        1
    );
    assert_eq!(conversation.revision().await, 1);
    let saved = store
        .get_conversation(Some("test-flow"), "durable-turn")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(
        serde_json::to_value(saved.messages).unwrap(),
        serde_json::to_value(history).unwrap()
    );
}

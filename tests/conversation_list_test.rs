//! Regression tests for `StateStore::list_conversations` and
//! `StateStore::count_conversations`, added with Phase-1 HITL support.

use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::sessions::ConversationRecord;
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::ChatMessage;

fn fresh_store() -> (tempfile::TempDir, JsonFileStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();
    (dir, store)
}

fn conv(
    id: &str,
    flow_path: Option<&str>,
    updated_at: &str,
    user_turns: usize,
) -> ConversationRecord {
    let mut messages = vec![ChatMessage::system("sys")];
    for i in 0..user_turns {
        messages.push(ChatMessage::user(&format!("user {}", i)));
        messages.push(ChatMessage::assistant(
            Some(format!("assistant {}", i)),
            None,
        ));
    }
    ConversationRecord {
        id: id.into(),
        flow_name: "flow".into(),
        flow_path: flow_path.map(|s| s.into()),
        agent_name: "assistant".into(),
        messages,
        created_at: "2026-04-09T08:00:00Z".into(),
        updated_at: updated_at.into(),
    }
}

#[tokio::test]
async fn list_conversations_returns_ordered_summaries() {
    let (_dir, store) = fresh_store();

    // Seed: three conversations, two in flow "chat-cli", one in "other".
    store
        .save_conversation(&conv("a", Some("chat-cli"), "2026-04-09T10:00:00Z", 2))
        .await
        .unwrap();
    store
        .save_conversation(&conv("b", Some("chat-cli"), "2026-04-09T12:00:00Z", 3))
        .await
        .unwrap();
    store
        .save_conversation(&conv("c", Some("other"), "2026-04-09T09:00:00Z", 1))
        .await
        .unwrap();

    // Filter to chat-cli, newest first.
    let listed = store
        .list_conversations(Some("chat-cli"), 10, 0)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "b");
    assert_eq!(listed[0].turn_count, 3);
    assert_eq!(listed[1].id, "a");
    assert_eq!(listed[1].turn_count, 2);

    // Without a filter: all three.
    let all = store.list_conversations(None, 10, 0).await.unwrap();
    assert_eq!(all.len(), 3);

    // Offset skips the newest.
    let paged = store
        .list_conversations(Some("chat-cli"), 10, 1)
        .await
        .unwrap();
    assert_eq!(paged.len(), 1);
    assert_eq!(paged[0].id, "a");

    // Limit caps the page size.
    let limited = store
        .list_conversations(Some("chat-cli"), 1, 0)
        .await
        .unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].id, "b");
}

#[tokio::test]
async fn count_conversations_matches_list() {
    let (_dir, store) = fresh_store();

    store
        .save_conversation(&conv("a", Some("chat-cli"), "2026-04-09T10:00:00Z", 2))
        .await
        .unwrap();
    store
        .save_conversation(&conv("b", Some("chat-cli"), "2026-04-09T12:00:00Z", 3))
        .await
        .unwrap();
    store
        .save_conversation(&conv("c", Some("other"), "2026-04-09T09:00:00Z", 1))
        .await
        .unwrap();
    store
        .save_conversation(&conv("d", None, "2026-04-09T11:00:00Z", 0))
        .await
        .unwrap();

    assert_eq!(
        store.count_conversations(Some("chat-cli")).await.unwrap(),
        2
    );
    assert_eq!(store.count_conversations(Some("other")).await.unwrap(), 1);
    // Records without a flow_path are invisible to any flow-scoped filter.
    assert_eq!(store.count_conversations(Some("missing")).await.unwrap(), 0);
    // No filter counts everything, including legacy records with no flow_path.
    assert_eq!(store.count_conversations(None).await.unwrap(), 4);
}

#[tokio::test]
async fn legacy_records_without_flow_path_are_invisible_to_filter() {
    let (_dir, store) = fresh_store();

    store
        .save_conversation(&conv("legacy", None, "2026-04-09T10:00:00Z", 1))
        .await
        .unwrap();

    let filtered = store
        .list_conversations(Some("anything"), 10, 0)
        .await
        .unwrap();
    assert!(filtered.is_empty());

    // But still reachable by direct id.
    let direct = store.get_conversation(None, "legacy").await.unwrap();
    assert!(direct.is_some());
}

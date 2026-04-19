//! Round-trip tests for the session persistence layer on the JSON backend.
//!
//! These tests exercise the `StateStore` session methods directly — the
//! higher-level Lua wiring is covered by the example project. The ID
//! validator has its own tests inside `src/engine/sessions.rs`.

use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::sessions::{ConversationRecord, DialogStateRecord};
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::ChatMessage;
use ironcrew::lua::dialog::DialogTurn;

fn fresh_store() -> (tempfile::TempDir, JsonFileStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();
    (dir, store)
}

fn sample_conversation(id: &str) -> ConversationRecord {
    ConversationRecord {
        id: id.into(),
        flow_name: "test flow".into(),
        flow_path: None,
        agent_name: "assistant".into(),
        messages: vec![
            ChatMessage::system("You are helpful"),
            ChatMessage::user("Hi"),
            ChatMessage::assistant(Some("Hello!".into()), None),
        ],
        created_at: "2026-04-09T08:00:00Z".into(),
        updated_at: "2026-04-09T08:00:01Z".into(),
    }
}

fn sample_dialog(id: &str) -> DialogStateRecord {
    DialogStateRecord {
        id: id.into(),
        flow_name: "debate".into(),
        flow_path: None,
        agent_names: vec!["alice".into(), "bob".into()],
        starter: "Debate the topic".into(),
        transcript: vec![
            DialogTurn {
                index: 0,
                speaker_index: 0,
                agent_name: "alice".into(),
                content: "I think X".into(),
                reasoning: None,
            },
            DialogTurn {
                index: 1,
                speaker_index: 1,
                agent_name: "bob".into(),
                content: "I disagree because Y".into(),
                reasoning: Some("user wanted disagreement".into()),
            },
        ],
        next_index: 2,
        stopped: false,
        stop_reason: None,
        created_at: "2026-04-09T08:00:00Z".into(),
        updated_at: "2026-04-09T08:00:02Z".into(),
    }
}

#[tokio::test]
async fn conversation_round_trip() {
    let (_dir, store) = fresh_store();
    let record = sample_conversation("chat-round-trip");

    store.save_conversation(&record).await.unwrap();
    let loaded = store
        .get_conversation(None, "chat-round-trip")
        .await
        .unwrap()
        .expect("saved record should load");

    assert_eq!(loaded.id, "chat-round-trip");
    assert_eq!(loaded.agent_name, "assistant");
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.messages[1].role, "user");
    assert_eq!(loaded.messages[1].content.as_deref(), Some("Hi"));
    assert_eq!(loaded.messages[2].role, "assistant");
    assert_eq!(loaded.messages[2].content.as_deref(), Some("Hello!"));
}

#[tokio::test]
async fn conversation_missing_returns_none() {
    let (_dir, store) = fresh_store();
    let result = store.get_conversation(None, "never-saved").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn conversation_save_is_upsert() {
    let (_dir, store) = fresh_store();
    let mut record = sample_conversation("upsert-chat");
    store.save_conversation(&record).await.unwrap();

    // Modify and re-save under the same id
    record.messages.push(ChatMessage::user("Round 2"));
    record.updated_at = "2026-04-09T09:00:00Z".into();
    store.save_conversation(&record).await.unwrap();

    let loaded = store
        .get_conversation(None, "upsert-chat")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.messages.len(), 4);
    assert_eq!(loaded.updated_at, "2026-04-09T09:00:00Z");
}

#[tokio::test]
async fn conversation_delete() {
    let (_dir, store) = fresh_store();
    let record = sample_conversation("delete-me");
    store.save_conversation(&record).await.unwrap();

    store.delete_conversation(None, "delete-me").await.unwrap();
    assert!(
        store
            .get_conversation(None, "delete-me")
            .await
            .unwrap()
            .is_none()
    );

    // Deleting a missing id is a no-op, not an error
    store
        .delete_conversation(None, "never-existed")
        .await
        .unwrap();
}

#[tokio::test]
async fn dialog_state_round_trip() {
    let (_dir, store) = fresh_store();
    let record = sample_dialog("debate-42");

    store.save_dialog_state(&record).await.unwrap();
    let loaded = store
        .get_dialog_state(None, "debate-42")
        .await
        .unwrap()
        .expect("saved record should load");

    assert_eq!(loaded.id, "debate-42");
    assert_eq!(loaded.agent_names, vec!["alice".to_string(), "bob".into()]);
    assert_eq!(loaded.transcript.len(), 2);
    assert_eq!(loaded.next_index, 2);
    assert!(!loaded.stopped);
    assert!(loaded.stop_reason.is_none());
    assert_eq!(
        loaded.transcript[1].reasoning.as_deref(),
        Some("user wanted disagreement")
    );
}

#[tokio::test]
async fn dialog_state_preserves_stop_flag() {
    let (_dir, store) = fresh_store();
    let mut record = sample_dialog("stopped-early");
    record.stopped = true;
    record.stop_reason = Some("consensus reached".into());

    store.save_dialog_state(&record).await.unwrap();
    let loaded = store
        .get_dialog_state(None, "stopped-early")
        .await
        .unwrap()
        .unwrap();

    assert!(loaded.stopped);
    assert_eq!(loaded.stop_reason.as_deref(), Some("consensus reached"));
}

#[tokio::test]
async fn dialog_state_missing_returns_none() {
    let (_dir, store) = fresh_store();
    assert!(
        store
            .get_dialog_state(None, "never-saved")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn dialog_state_save_is_upsert() {
    let (_dir, store) = fresh_store();
    let mut record = sample_dialog("upsert-dialog");
    store.save_dialog_state(&record).await.unwrap();

    // Simulate a new turn being appended and saved again
    record.transcript.push(DialogTurn {
        index: 2,
        speaker_index: 0,
        agent_name: "alice".into(),
        content: "Back to me".into(),
        reasoning: None,
    });
    record.next_index = 3;
    record.updated_at = "2026-04-09T10:00:00Z".into();
    store.save_dialog_state(&record).await.unwrap();

    let loaded = store
        .get_dialog_state(None, "upsert-dialog")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.transcript.len(), 3);
    assert_eq!(loaded.next_index, 3);
}

#[tokio::test]
async fn dialog_state_delete() {
    let (_dir, store) = fresh_store();
    let record = sample_dialog("delete-dialog");
    store.save_dialog_state(&record).await.unwrap();

    store
        .delete_dialog_state(None, "delete-dialog")
        .await
        .unwrap();
    assert!(
        store
            .get_dialog_state(None, "delete-dialog")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn runs_and_sessions_are_independent() {
    // Save one of each record type and verify we can still load each by id
    // (i.e., they live in separate "namespaces" within the same store).
    let (_dir, store) = fresh_store();
    store
        .save_conversation(&sample_conversation("shared-id"))
        .await
        .unwrap();
    store
        .save_dialog_state(&sample_dialog("shared-id"))
        .await
        .unwrap();

    // A conversation and a dialog with the same id can coexist because
    // they're stored separately.
    let conv = store.get_conversation(None, "shared-id").await.unwrap();
    let dlg = store.get_dialog_state(None, "shared-id").await.unwrap();
    assert!(conv.is_some());
    assert!(dlg.is_some());
    assert_eq!(conv.unwrap().agent_name, "assistant");
    assert_eq!(
        dlg.unwrap().agent_names,
        vec!["alice".to_string(), "bob".into()]
    );
}

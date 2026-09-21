use super::*;
use ironcrew::engine::sessions::{ConversationExecution, ConversationRecord, DialogStateRecord};
use ironcrew::llm::provider::ChatMessage;

pub(super) async fn checked_round_trip(store: &dyn StateStore) {
    let scope = UsageTracker::default();
    scope
        .start()
        .unwrap()
        .finish(UsageReceipt::from_counts(
            UsageCounts {
                prompt_tokens: Some(u64::MAX - 10),
                completion_tokens: Some(10),
                total_tokens: Some(u64::MAX),
                reasoning_tokens: Some(7),
                cached_tokens: Some(3),
                ..Default::default()
            },
            true,
        ))
        .unwrap();
    scope
        .start()
        .unwrap()
        .finish(UsageReceipt::default())
        .unwrap();
    let usage = scope.snapshot().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let mut chat = ConversationRecord {
        id: id.clone(),
        flow_name: "session usage".into(),
        flow_path: Some("usage".into()),
        agent_name: "alice".into(),
        execution: ConversationExecution::new(
            format!("sha256:{}", "1".repeat(64)),
            format!("sha256:{}", "2".repeat(64)),
            10,
            1024 * 1024,
        )
        .unwrap(),
        messages: vec![ChatMessage::system("test")],
        created_at: now.clone(),
        updated_at: now.clone(),
        revision: 0,
        usage: usage.clone(),
    };
    let mut dialog = DialogStateRecord {
        id: id.clone(),
        flow_name: "session usage".into(),
        flow_path: Some("usage".into()),
        agent_names: vec!["alice".into(), "bob".into()],
        starter: "test".into(),
        transcript: vec![],
        next_index: 0,
        stopped: false,
        stop_reason: None,
        created_at: now.clone(),
        updated_at: now,
        revision: 0,
        usage: usage.clone(),
    };
    chat.revision = store.save_conversation(&chat).await.unwrap();
    dialog.revision = store.save_dialog_state(&dialog).await.unwrap();
    // Both insert and revision-checked update paths carry the entire snapshot.
    chat.revision = store.save_conversation(&chat).await.unwrap();
    dialog.revision = store.save_dialog_state(&dialog).await.unwrap();
    let loaded = store
        .get_conversation(Some("usage"), &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.usage, usage);
    assert_eq!(loaded.revision, 2);
    assert_eq!(
        store
            .get_dialog_state(Some("usage"), &id)
            .await
            .unwrap()
            .unwrap()
            .usage,
        usage
    );
    assert_eq!(
        store
            .list_conversations(Some("usage"), 100, 0)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap()
            .usage,
        usage
    );
    let mut stale = chat.clone();
    stale.revision = 1;
    stale.usage = Default::default();
    assert!(store.save_conversation(&stale).await.is_err());
    let mut stale = dialog.clone();
    stale.revision = 1;
    stale.usage = Default::default();
    assert!(store.save_dialog_state(&stale).await.is_err());
    chat.usage.coverage = UsageCoverage::Complete;
    dialog.usage.coverage = UsageCoverage::Complete;
    assert!(store.save_conversation(&chat).await.is_err());
    assert!(store.save_dialog_state(&dialog).await.is_err());
    let active = UsageTracker::default();
    let attempt = active.start().unwrap();
    chat.usage = active.snapshot().unwrap();
    dialog.usage = chat.usage.clone();
    assert!(store.save_conversation(&chat).await.is_err());
    assert!(store.save_dialog_state(&dialog).await.is_err());
    drop(attempt);
    assert_eq!(
        store
            .get_conversation(Some("usage"), &id)
            .await
            .unwrap()
            .unwrap()
            .usage,
        usage
    );
    assert_eq!(
        store
            .get_dialog_state(Some("usage"), &id)
            .await
            .unwrap()
            .unwrap()
            .usage,
        usage
    );
}

pub(super) async fn sqlite_corruption(store: &dyn StateStore, conn: &rusqlite::Connection) {
    let record = store
        .list_conversations(Some("usage"), 100, 0)
        .await
        .unwrap()
        .remove(0);
    let active = UsageTracker::default();
    let _attempt = active.start().unwrap();
    for raw in [
        "x".repeat(4097),
        "{}".into(),
        serde_json::to_string(&active.snapshot().unwrap()).unwrap(),
    ] {
        conn.execute("UPDATE conversations SET usage = ?1", [&raw])
            .unwrap();
        conn.execute("UPDATE dialogs SET usage = ?1", [&raw])
            .unwrap();
        assert!(
            store
                .get_conversation(Some("usage"), &record.id)
                .await
                .is_err()
        );
        assert!(
            store
                .list_conversations(Some("usage"), 100, 0)
                .await
                .is_err()
        );
        assert!(
            store
                .get_dialog_state(Some("usage"), &record.id)
                .await
                .is_err()
        );
    }
}

#[cfg(feature = "postgres")]
pub(super) async fn postgres_corruption(store: &dyn StateStore, pool: &sqlx::PgPool) {
    let record = store
        .list_conversations(Some("usage"), 100, 0)
        .await
        .unwrap()
        .remove(0);
    let active = UsageTracker::default();
    let _attempt = active.start().unwrap();
    for value in [
        serde_json::json!({"oversized":"x".repeat(4097)}),
        serde_json::json!({}),
        serde_json::to_value(active.snapshot().unwrap()).unwrap(),
    ] {
        sqlx::query("UPDATE ic046_usage_conversations SET usage = $1")
            .bind(sqlx::types::Json(value.clone()))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE ic046_usage_dialogs SET usage = $1")
            .bind(sqlx::types::Json(value))
            .execute(pool)
            .await
            .unwrap();
        assert!(
            store
                .get_conversation(Some("usage"), &record.id)
                .await
                .is_err()
        );
        assert!(
            store
                .list_conversations(Some("usage"), 100, 0)
                .await
                .is_err()
        );
        assert!(
            store
                .get_dialog_state(Some("usage"), &record.id)
                .await
                .is_err()
        );
    }
    sqlx::query("DELETE FROM ic046_usage_conversations")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM ic046_usage_dialogs")
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn sqlite_upgrade_marks_old_sessions_unavailable_without_inventing_receipts() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("upgrade.db");
    let store = SqliteStore::new(path.clone()).unwrap();
    checked_round_trip(&store).await;
    let saved = store
        .list_conversations(Some("usage"), 100, 0)
        .await
        .unwrap()
        .remove(0);
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("ALTER TABLE conversations DROP COLUMN usage", [])
        .unwrap();
    conn.execute("ALTER TABLE dialogs DROP COLUMN usage", [])
        .unwrap();
    drop(conn);
    let store = SqliteStore::new(path).unwrap();
    let chat = store
        .get_conversation(Some("usage"), &saved.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chat.usage, UsageSnapshot::unavailable());
    let dialog = store
        .get_dialog_state(Some("usage"), &saved.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dialog.usage, UsageSnapshot::unavailable());
    let mut old_json = serde_json::to_value(chat).unwrap();
    old_json.as_object_mut().unwrap().remove("usage");
    assert!(serde_json::from_value::<ConversationRecord>(old_json).is_err());
}

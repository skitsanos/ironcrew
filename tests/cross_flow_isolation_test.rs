//! Regression tests for cross-flow conversation **and dialog** isolation.
//!
//! These tests lock in the contract that the effective unique key for
//! persisted conversations and dialogs is `(flow_path, id)`. Two
//! sessions with the same `id` but different `flow_path` must never
//! read or delete each other's state.

use ironcrew::engine::conversation_json::HARD_STORED_CONVERSATION_JSON_DEPTH;
use ironcrew::engine::conversation_record::{
    HARD_STORED_CONVERSATION_EXECUTION_BYTES, HARD_STORED_CONVERSATION_MESSAGES,
    HARD_STORED_CONVERSATION_METADATA_BYTES,
};
use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::sessions::{
    CONVERSATION_EXECUTION_SCHEMA_VERSION, ConversationExecution, ConversationRecord,
    DialogStateRecord,
};
use ironcrew::engine::sqlite_store::SqliteStore;
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::ChatMessage;

fn record(id: &str, flow_path: &str) -> ConversationRecord {
    ConversationRecord {
        id: id.to_string(),
        flow_name: format!("{}-goal", flow_path),
        flow_path: Some(flow_path.to_string()),
        agent_name: "tutor".to_string(),
        execution: conversation_execution(),
        messages: vec![ChatMessage::system("sys")],
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        revision: 0,
    }
}

fn conversation_execution() -> ConversationExecution {
    ConversationExecution {
        schema_version: CONVERSATION_EXECUTION_SCHEMA_VERSION,
        incarnation_id: "00000000-0000-4000-8000-000000000001".into(),
        source_fingerprint: format!("sha256:{}", "1".repeat(64)),
        definition_fingerprint: format!("sha256:{}", "2".repeat(64)),
        max_history: 50,
        history_max_bytes: 1024 * 1024,
    }
}

fn deeply_nested_messages() -> String {
    let nested = format!(
        "{}null{}",
        "[".repeat(HARD_STORED_CONVERSATION_JSON_DEPTH),
        "]".repeat(HARD_STORED_CONVERSATION_JSON_DEPTH)
    );
    format!(
        r#"[{{"role":"system","content":"sys"}},{{"role":"user","content":"hi"}},{{"role":"assistant","content":null,"raw_blocks":[{nested}]}}]"#
    )
}

#[tokio::test]
async fn get_is_scoped_by_flow_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    // Same id, two flows.
    store
        .save_conversation(&record("shared", "chat-a"))
        .await
        .unwrap();
    store
        .save_conversation(&record("shared", "chat-b"))
        .await
        .unwrap();

    // `chat-a` sees its own record — not `chat-b`'s.
    let got_a = store
        .get_conversation(Some("chat-a"), "shared")
        .await
        .unwrap()
        .expect("chat-a record missing");
    assert_eq!(got_a.flow_path.as_deref(), Some("chat-a"));

    // `chat-b` sees its own — and note that JSON backend stores both
    // under `shared.json` (last write wins). The important invariant
    // under the flow-scoped API is: the caller never receives a record
    // belonging to another flow.
    let got_b = store
        .get_conversation(Some("chat-b"), "shared")
        .await
        .unwrap();
    // Depending on write order, `chat-b` either matches its own record
    // or the lookup returns None (because `chat-a` overwrote). Both
    // outcomes keep the cross-flow isolation invariant.
    if let Some(r) = got_b {
        assert_eq!(r.flow_path.as_deref(), Some("chat-b"));
    }

    // A foreign flow must never see a record that doesn't belong to it.
    let got_c = store
        .get_conversation(Some("some-other-flow"), "shared")
        .await
        .unwrap();
    assert!(got_c.is_none());
}

#[tokio::test]
async fn delete_is_scoped_by_flow_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    store
        .save_conversation(&record("target", "chat-a"))
        .await
        .unwrap();

    // A delete from the wrong flow is a silent no-op.
    store
        .delete_conversation(Some("chat-b"), "target")
        .await
        .unwrap();

    // Record still exists in the owning flow.
    let still_there = store
        .get_conversation(Some("chat-a"), "target")
        .await
        .unwrap();
    assert!(
        still_there.is_some(),
        "cross-flow delete removed another flow's record"
    );

    // A delete from the correct flow does remove it.
    store
        .delete_conversation(Some("chat-a"), "target")
        .await
        .unwrap();
    let gone = store
        .get_conversation(Some("chat-a"), "target")
        .await
        .unwrap();
    assert!(gone.is_none());
}

#[tokio::test]
async fn legacy_records_are_invisible_to_scoped_queries() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    // Legacy record with flow_path = None (pre-2.13).
    let mut legacy = record("legacy", "chat-a");
    legacy.flow_path = None;
    store.save_conversation(&legacy).await.unwrap();

    // Flow-scoped lookup returns None (safety posture — legacy records
    // cannot be attributed to any specific flow).
    let scoped = store
        .get_conversation(Some("chat-a"), "legacy")
        .await
        .unwrap();
    assert!(scoped.is_none());

    // Admin (global) lookup returns the record — preserves access for
    // the `ironcrew inspect` CLI and similar paths.
    let global = store.get_conversation(None, "legacy").await.unwrap();
    assert!(global.is_some());
}

// ── Dialog parity ──────────────────────────────────────────────────────────
//
// Mirror the conversation invariants for dialogs. Before this release dialogs
// were globally keyed by `id` alone; a session saved from flow A could be
// resumed from flow B. These tests lock in the fix.

fn dialog_record(id: &str, flow_path: &str) -> DialogStateRecord {
    DialogStateRecord {
        id: id.to_string(),
        flow_name: format!("{}-goal", flow_path),
        flow_path: Some(flow_path.to_string()),
        agent_names: vec!["alice".into(), "bob".into()],
        starter: "Hello".to_string(),
        transcript: vec![],
        next_index: 0,
        stopped: false,
        stop_reason: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        revision: 0,
    }
}

#[tokio::test]
async fn dialog_get_is_scoped_by_flow_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    store
        .save_dialog_state(&dialog_record("shared-dlg", "flow-a"))
        .await
        .unwrap();

    // Foreign flow must not see another flow's dialog.
    let foreign = store
        .get_dialog_state(Some("flow-b"), "shared-dlg")
        .await
        .unwrap();
    assert!(foreign.is_none(), "flow-b leaked flow-a's dialog");

    // Owning flow resumes cleanly.
    let own = store
        .get_dialog_state(Some("flow-a"), "shared-dlg")
        .await
        .unwrap()
        .expect("flow-a should see its own dialog");
    assert_eq!(own.flow_path.as_deref(), Some("flow-a"));
}

#[tokio::test]
async fn dialog_delete_is_scoped_by_flow_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    store
        .save_dialog_state(&dialog_record("target-dlg", "flow-a"))
        .await
        .unwrap();

    // Wrong flow: silent no-op.
    store
        .delete_dialog_state(Some("flow-b"), "target-dlg")
        .await
        .unwrap();
    let still_there = store
        .get_dialog_state(Some("flow-a"), "target-dlg")
        .await
        .unwrap();
    assert!(
        still_there.is_some(),
        "cross-flow delete removed another flow's dialog"
    );

    // Right flow: succeeds.
    store
        .delete_dialog_state(Some("flow-a"), "target-dlg")
        .await
        .unwrap();
    let gone = store
        .get_dialog_state(Some("flow-a"), "target-dlg")
        .await
        .unwrap();
    assert!(gone.is_none());
}

#[tokio::test]
async fn dialog_legacy_records_invisible_to_scoped_queries() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();

    let mut legacy = dialog_record("legacy-dlg", "flow-a");
    legacy.flow_path = None;
    store.save_dialog_state(&legacy).await.unwrap();

    let scoped = store
        .get_dialog_state(Some("flow-a"), "legacy-dlg")
        .await
        .unwrap();
    assert!(scoped.is_none());

    let global = store.get_dialog_state(None, "legacy-dlg").await.unwrap();
    assert!(
        global.is_some(),
        "global lookup should still find legacy dialogs"
    );
}

// ── SQLite backend parity ──────────────────────────────────────────────────
//
// Same contract as the JSON backend, but exercising SQLite's composite
// `(flow_path, id)` unique constraint. This catches regressions in SQL
// upsert semantics — which Codex flagged as uncovered by the JSON-only
// tests above.

fn sqlite_in_tempdir() -> (tempfile::TempDir, SqliteStore) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("isolation.db");
    let store = SqliteStore::new(db).unwrap();
    (dir, store)
}

#[tokio::test]
async fn sqlite_conversation_cross_flow_isolation() {
    let (_dir, store) = sqlite_in_tempdir();

    // Two flows save a session with the SAME id — the SQL layer must
    // keep them as independent rows, not let one overwrite the other.
    store
        .save_conversation(&record("shared", "flow-a"))
        .await
        .unwrap();
    store
        .save_conversation(&record("shared", "flow-b"))
        .await
        .unwrap();

    let a = store
        .get_conversation(Some("flow-a"), "shared")
        .await
        .unwrap()
        .expect("flow-a conversation was overwritten");
    assert_eq!(a.flow_path.as_deref(), Some("flow-a"));

    let b = store
        .get_conversation(Some("flow-b"), "shared")
        .await
        .unwrap()
        .expect("flow-b conversation missing");
    assert_eq!(b.flow_path.as_deref(), Some("flow-b"));

    // Delete in flow-a must not touch flow-b's row.
    store
        .delete_conversation(Some("flow-a"), "shared")
        .await
        .unwrap();
    assert!(
        store
            .get_conversation(Some("flow-a"), "shared")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_conversation(Some("flow-b"), "shared")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn sqlite_dialog_cross_flow_isolation() {
    let (_dir, store) = sqlite_in_tempdir();

    store
        .save_dialog_state(&dialog_record("shared-dlg", "flow-a"))
        .await
        .unwrap();
    store
        .save_dialog_state(&dialog_record("shared-dlg", "flow-b"))
        .await
        .unwrap();

    let a = store
        .get_dialog_state(Some("flow-a"), "shared-dlg")
        .await
        .unwrap()
        .expect("flow-a dialog was overwritten");
    assert_eq!(a.flow_path.as_deref(), Some("flow-a"));

    let b = store
        .get_dialog_state(Some("flow-b"), "shared-dlg")
        .await
        .unwrap()
        .expect("flow-b dialog missing");
    assert_eq!(b.flow_path.as_deref(), Some("flow-b"));

    // Scoped delete must only hit its own flow.
    store
        .delete_dialog_state(Some("flow-a"), "shared-dlg")
        .await
        .unwrap();
    assert!(
        store
            .get_dialog_state(Some("flow-a"), "shared-dlg")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_dialog_state(Some("flow-b"), "shared-dlg")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn sqlite_legacy_records_invisible_to_scoped_queries() {
    let (_dir, store) = sqlite_in_tempdir();

    // Legacy record with flow_path = None (pre-migration data).
    let mut legacy_conv = record("legacy", "flow-a");
    legacy_conv.flow_path = None;
    legacy_conv.revision = store.save_conversation(&legacy_conv).await.unwrap();

    let mut legacy_dlg = dialog_record("legacy-dlg", "flow-a");
    legacy_dlg.flow_path = None;
    store.save_dialog_state(&legacy_dlg).await.unwrap();

    // Scoped lookups miss legacy records (by design).
    assert!(
        store
            .get_conversation(Some("flow-a"), "legacy")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_dialog_state(Some("flow-a"), "legacy-dlg")
            .await
            .unwrap()
            .is_none()
    );

    // Global lookups find them (admin / inspect path).
    assert!(
        store
            .get_conversation(None, "legacy")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .get_dialog_state(None, "legacy-dlg")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn sqlite_null_scoped_saves_remain_true_upserts() {
    let (dir, store) = sqlite_in_tempdir();

    let mut legacy_conv = record("legacy-upsert", "flow-a");
    legacy_conv.flow_path = None;
    legacy_conv.updated_at = "2026-01-01T00:00:00Z".into();
    legacy_conv.revision = store.save_conversation(&legacy_conv).await.unwrap();
    legacy_conv.updated_at = "2026-01-01T00:00:05Z".into();
    store.save_conversation(&legacy_conv).await.unwrap();

    let loaded_conv = store
        .get_conversation(None, "legacy-upsert")
        .await
        .unwrap()
        .expect("global conversation lookup should find updated legacy row");
    assert_eq!(loaded_conv.updated_at, "2026-01-01T00:00:05Z");

    let conn = rusqlite::Connection::open(dir.path().join("isolation.db")).unwrap();
    let conv_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM conversations WHERE id = ?1 AND flow_path IS NULL",
            rusqlite::params!["legacy-upsert"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(conv_count, 1, "NULL-scoped conversation save should upsert");

    let mut legacy_dlg = dialog_record("legacy-upsert-dlg", "flow-a");
    legacy_dlg.flow_path = None;
    legacy_dlg.updated_at = "2026-01-01T00:00:00Z".into();
    legacy_dlg.revision = store.save_dialog_state(&legacy_dlg).await.unwrap();
    legacy_dlg.updated_at = "2026-01-01T00:00:05Z".into();
    legacy_dlg.revision = store.save_dialog_state(&legacy_dlg).await.unwrap();

    let loaded_dlg = store
        .get_dialog_state(None, "legacy-upsert-dlg")
        .await
        .unwrap()
        .expect("global dialog lookup should find updated legacy row");
    assert_eq!(loaded_dlg.updated_at, "2026-01-01T00:00:05Z");

    let dlg_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dialogs WHERE id = ?1 AND flow_path IS NULL",
            rusqlite::params!["legacy-upsert-dlg"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dlg_count, 1, "NULL-scoped dialog save should upsert");
}

#[tokio::test]
async fn sqlite_rejects_stale_session_snapshots() {
    let (_dir, store) = sqlite_in_tempdir();

    let mut conversation = record("sqlite-cas", "flow-a");
    conversation.revision = store.save_conversation(&conversation).await.unwrap();
    let stale_conversation = conversation.clone();
    conversation.updated_at = "2026-01-01T00:00:01Z".into();
    conversation.revision = store.save_conversation(&conversation).await.unwrap();
    assert!(matches!(
        store.save_conversation(&stale_conversation).await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));

    let mut dialog = dialog_record("sqlite-dialog-cas", "flow-a");
    dialog.revision = store.save_dialog_state(&dialog).await.unwrap();
    let stale_dialog = dialog.clone();
    dialog.updated_at = "2026-01-01T00:00:01Z".into();
    dialog.revision = store.save_dialog_state(&dialog).await.unwrap();
    assert!(matches!(
        store.save_dialog_state(&stale_dialog).await,
        Err(ironcrew::utils::error::IronCrewError::Conflict(_))
    ));
}

#[tokio::test]
async fn json_store_preflights_untrusted_conversation_files_before_decode() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();
    store
        .save_conversation(&record("nested-json", "flow-a"))
        .await
        .unwrap();

    let execution = serde_json::to_string(&conversation_execution()).unwrap();
    let messages = deeply_nested_messages();
    let raw = format!(
        r#"{{"id":"nested-json","flow_name":"flow-a-goal","flow_path":"flow-a","agent_name":"tutor","execution":{execution},"messages":{messages},"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","revision":1}}"#
    );
    std::fs::write(
        dir.path()
            .join("conversations")
            .join("flow-a")
            .join("nested-json.json"),
        raw,
    )
    .unwrap();

    let error = store
        .get_conversation(Some("flow-a"), "nested-json")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("hard JSON nesting-depth limit"));
    assert!(
        store
            .list_conversations(Some("flow-a"), 10, 0)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.count_conversations(Some("flow-a")).await.unwrap(), 0);
}

#[tokio::test]
async fn sqlite_bounds_untrusted_conversation_rows_before_decode() {
    let (dir, store) = sqlite_in_tempdir();
    let conn = rusqlite::Connection::open(dir.path().join("isolation.db")).unwrap();
    let insert = "INSERT INTO conversations \
        (id, flow_name, flow_path, agent_name, execution, messages, created_at, updated_at, revision) \
        VALUES (?1, 'chat', 'flow-a', 'assistant', ?2, ?3, \
                '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 1)";

    let oversized_execution = serde_json::json!({
        "sentinel": "x".repeat(HARD_STORED_CONVERSATION_EXECUTION_BYTES + 1),
    })
    .to_string();
    conn.execute(
        insert,
        rusqlite::params!["oversized-execution", oversized_execution, "[]"],
    )
    .unwrap();
    let error = store
        .get_conversation(Some("flow-a"), "oversized-execution")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("execution identity exceeds the hard byte limit"));
    assert!(!error.contains("sentinel"));
    store
        .delete_conversation(Some("flow-a"), "oversized-execution")
        .await
        .unwrap();

    let execution = serde_json::to_string(&conversation_execution()).unwrap();
    let too_many_messages = serde_json::to_string(&vec![
        serde_json::json!({});
        HARD_STORED_CONVERSATION_MESSAGES + 1
    ])
    .unwrap();
    conn.execute(
        insert,
        rusqlite::params!["oversized-count", execution, too_many_messages],
    )
    .unwrap();
    let error = store
        .get_conversation(Some("flow-a"), "oversized-count")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("message count exceeds the hard limit"));
    let list_error = store
        .list_conversations(Some("flow-a"), 10, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        list_error.contains("SQLite stored conversation summary is corrupt or exceeds hard limits")
    );
    store
        .delete_conversation(Some("flow-a"), "oversized-count")
        .await
        .unwrap();

    let execution = serde_json::to_string(&conversation_execution()).unwrap();
    conn.execute(
        insert,
        rusqlite::params!["corrupt-shape", execution, "{not-json"],
    )
    .unwrap();
    let error = store
        .get_conversation(Some("flow-a"), "corrupt-shape")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("messages must be a JSON array"));
    let list_error = store
        .list_conversations(Some("flow-a"), 10, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        list_error.contains("SQLite stored conversation summary is corrupt or exceeds hard limits")
    );
    store
        .delete_conversation(Some("flow-a"), "corrupt-shape")
        .await
        .unwrap();

    let execution = serde_json::to_string(&conversation_execution()).unwrap();
    conn.execute(
        insert,
        rusqlite::params!["nested-raw-blocks", execution, deeply_nested_messages()],
    )
    .unwrap();
    let error = store
        .get_conversation(Some("flow-a"), "nested-raw-blocks")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("hard JSON nesting-depth limit"));
    let summaries = store
        .list_conversations(Some("flow-a"), 10, 0)
        .await
        .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].turn_count, 1);
    store
        .delete_conversation(Some("flow-a"), "nested-raw-blocks")
        .await
        .unwrap();

    let execution = serde_json::to_string(&conversation_execution()).unwrap();
    conn.execute(
        insert,
        rusqlite::params!["oversized-summary", execution, "[]"],
    )
    .unwrap();
    let oversized_agent = format!(
        "summary-sentinel{}",
        "x".repeat(HARD_STORED_CONVERSATION_METADATA_BYTES)
    );
    conn.execute(
        "UPDATE conversations SET agent_name = ?1 WHERE id = ?2",
        rusqlite::params![oversized_agent, "oversized-summary"],
    )
    .unwrap();
    let list_error = store
        .list_conversations(Some("flow-a"), 10, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        list_error.contains("SQLite stored conversation summary is corrupt or exceeds hard limits")
    );
    assert!(!list_error.contains("summary-sentinel"));
    store
        .delete_conversation(Some("flow-a"), "oversized-summary")
        .await
        .unwrap();
}

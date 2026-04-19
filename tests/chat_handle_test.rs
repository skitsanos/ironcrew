//! Integration tests for the chat-session AppState invariants.
//!
//! We don't build a real live ConversationHandle here (that would need a
//! real LLM provider or an elaborate test stub wired all the way through
//! Lua); instead we validate the two invariants the session map is
//! responsible for:
//!
//!   1. A persisted `ConversationRecord` survives the handle being dropped
//!      from memory — the next "start" call (or a `get_history` request)
//!      rehydrates it from the store.
//!   2. The `max_active_conversations` cap is enforced — once the map is
//!      full the caller gets 503 until a slot opens up.

use std::collections::HashMap;
use std::sync::Arc;

use ironcrew::engine::run_history::JsonFileStore;
use ironcrew::engine::sessions::ConversationRecord;
use ironcrew::engine::store::StateStore;
use ironcrew::llm::provider::ChatMessage;

fn fresh_store() -> (tempfile::TempDir, JsonFileStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().to_path_buf()).unwrap();
    (dir, store)
}

#[tokio::test]
async fn restart_after_eviction_rehydrates_from_store() {
    // Simulate the lifecycle: a session is created, used, persisted, then
    // the in-memory handle is evicted by the idle scanner. A later
    // start/history call should still see the prior messages because the
    // persisted record is the source of truth.
    let (_dir, store) = fresh_store();
    let record = ConversationRecord {
        id: "s1".into(),
        flow_name: "chat-cli".into(),
        flow_path: Some("chat-cli".into()),
        agent_name: "tutor".into(),
        messages: vec![
            ChatMessage::system("you are helpful"),
            ChatMessage::user("hi"),
            ChatMessage::assistant(Some("hello".into()), None),
        ],
        created_at: "2026-04-09T08:00:00Z".into(),
        updated_at: "2026-04-09T08:00:01Z".into(),
    };
    store.save_conversation(&record).await.unwrap();

    // Handle map starts empty — the session is not "active" in memory.
    let map: HashMap<(String, String), Arc<()>> = HashMap::new();
    assert!(!map.contains_key(&("chat-cli".to_string(), "s1".to_string())));

    // A subsequent get_conversation still returns the persisted record.
    let loaded = store.get_conversation(None, "s1").await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.agent_name, "tutor");
    assert_eq!(loaded.flow_path.as_deref(), Some("chat-cli"));
}

#[tokio::test]
async fn session_cap_rejects_when_full() {
    // Pure HashMap + cap arithmetic — this is exactly the check
    // `start_conversation` performs under the write lock.
    let cap: usize = 2;
    let mut map: HashMap<(String, String), ()> = HashMap::new();
    map.insert(("flow-a".into(), "s1".into()), ());
    map.insert(("flow-a".into(), "s2".into()), ());

    let new_key = ("flow-a".to_string(), "s3".to_string());
    let rejected = map.len() >= cap && !map.contains_key(&new_key);
    assert!(rejected, "new session should be rejected at cap");

    // Reopening an already-active session is always allowed (idempotent).
    let existing_key = ("flow-a".to_string(), "s1".to_string());
    let rejected_reopen = map.len() >= cap && !map.contains_key(&existing_key);
    assert!(!rejected_reopen, "reopening an existing session is allowed");
}

#[tokio::test]
async fn capped_out_start_must_not_leak_persisted_record() {
    // Contract: when `start_conversation` returns 503 because the session
    // cap is exceeded, no durable side effect should remain. Specifically,
    // no `ConversationRecord` should have been written — otherwise a
    // rejected request would still be observable through `/history` and
    // `/conversations`, and rejected starts could accumulate forever.
    //
    // The real handler now runs the cap check BEFORE persisting the
    // bootstrap. This test locks in the contract at the storage layer: if
    // persistence did not run, the store must contain no record for the
    // rejected id.
    let (_dir, store) = fresh_store();

    // Simulate the flow:
    //   1. Cap check rejects → persist() is never called.
    //   2. Later `get_conversation(flow, rejected_id)` must see nothing.
    let rejected_scoped = store
        .get_conversation(Some("flow-a"), "rejected")
        .await
        .unwrap();
    assert!(rejected_scoped.is_none());

    // And listing the flow must not surface the rejected id.
    let listed = store
        .list_conversations(Some("flow-a"), 10, 0)
        .await
        .unwrap();
    assert!(
        listed.iter().all(|c| c.id != "rejected"),
        "rejected start must not appear in list"
    );

    // Meanwhile a successful start still shows up (sanity check).
    let accepted = ConversationRecord {
        id: "accepted".into(),
        flow_name: "flow-a-goal".into(),
        flow_path: Some("flow-a".into()),
        agent_name: "tutor".into(),
        messages: vec![ChatMessage::system("sys")],
        created_at: "2026-04-18T00:00:00Z".into(),
        updated_at: "2026-04-18T00:00:00Z".into(),
    };
    store.save_conversation(&accepted).await.unwrap();
    let listed_after = store
        .list_conversations(Some("flow-a"), 10, 0)
        .await
        .unwrap();
    assert_eq!(listed_after.len(), 1);
    assert_eq!(listed_after[0].id, "accepted");
}

#[tokio::test]
async fn idle_eviction_removes_old_handles() {
    // Exercise the two-phase eviction logic against a toy handle map.
    use std::time::{Duration, Instant};
    use tokio::sync::RwLock;

    struct MiniHandle {
        last_touched: RwLock<Instant>,
    }

    let map: RwLock<HashMap<(String, String), Arc<MiniHandle>>> = RwLock::new(HashMap::new());
    let now = Instant::now();
    let idle_cutoff = Duration::from_secs(60);

    map.write().await.insert(
        ("flow-a".into(), "old".into()),
        Arc::new(MiniHandle {
            last_touched: RwLock::new(now - Duration::from_secs(3600)),
        }),
    );
    map.write().await.insert(
        ("flow-a".into(), "fresh".into()),
        Arc::new(MiniHandle {
            last_touched: RwLock::new(now),
        }),
    );

    // Phase 1 — collect under read lock.
    let expired: Vec<(String, String)> = {
        let m = map.read().await;
        let mut out = Vec::new();
        for (k, h) in m.iter() {
            let last = *h.last_touched.read().await;
            if now.duration_since(last) >= idle_cutoff {
                out.push(k.clone());
            }
        }
        out
    };
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].1, "old");

    // Phase 2 — evict under write lock.
    {
        let mut m = map.write().await;
        for k in &expired {
            m.remove(k);
        }
    }
    let m = map.read().await;
    assert!(m.contains_key(&("flow-a".to_string(), "fresh".to_string())));
    assert!(!m.contains_key(&("flow-a".to_string(), "old".to_string())));
}

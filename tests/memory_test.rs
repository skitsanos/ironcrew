use ironcrew::engine::memory::MemoryStore;
use serde_json::json;

#[tokio::test]
async fn test_memory_set_get() {
    let store = MemoryStore::ephemeral();
    store.set("key1".into(), json!("value1")).await;
    let val = store.get("key1").await;
    assert_eq!(val, Some(json!("value1")));
}

#[tokio::test]
async fn test_memory_get_missing() {
    let store = MemoryStore::ephemeral();
    let val = store.get("nonexistent").await;
    assert_eq!(val, None);
}

#[tokio::test]
async fn test_memory_delete() {
    let store = MemoryStore::ephemeral();
    store.set("key1".into(), json!("value1")).await;
    assert!(store.delete("key1").await);
    assert_eq!(store.get("key1").await, None);
}

#[tokio::test]
async fn test_memory_keys() {
    let store = MemoryStore::ephemeral();
    store.set("a".into(), json!(1)).await;
    store.set("b".into(), json!(2)).await;
    let mut keys = store.keys().await;
    keys.sort();
    assert_eq!(keys, vec!["a", "b"]);
}

#[tokio::test]
async fn test_memory_ttl_expiry() {
    let store = MemoryStore::ephemeral();
    store
        .set_with_options("temp".into(), json!("data"), vec![], Some(1))
        .await;
    // Sleep a bit to let it expire
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(store.get("temp").await, None);
}

#[tokio::test]
async fn test_memory_update_existing() {
    let store = MemoryStore::ephemeral();
    store.set("key".into(), json!("v1")).await;
    store.set("key".into(), json!("v2")).await;
    assert_eq!(store.get("key").await, Some(json!("v2")));
}

#[tokio::test]
async fn test_memory_clear() {
    let store = MemoryStore::ephemeral();
    store.set("a".into(), json!(1)).await;
    store.set("b".into(), json!(2)).await;
    store.clear().await;
    assert!(store.keys().await.is_empty());
}

#[tokio::test]
async fn test_memory_build_context() {
    let store = MemoryStore::ephemeral();
    store
        .set_with_options(
            "research".into(),
            json!("Rust is fast"),
            vec!["research".into()],
            None,
        )
        .await;
    store
        .set_with_options(
            "notes".into(),
            json!("Python is easy"),
            vec!["notes".into()],
            None,
        )
        .await;
    let ctx = store.build_context("research findings about Rust", 5).await;
    assert!(ctx.contains("Rust is fast"));
}

#[tokio::test]
async fn test_memory_persistent_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.json");

    // Write
    {
        let store = MemoryStore::persistent(path.clone()).unwrap();
        store.set("key1".into(), json!("value1")).await;
        store
            .set("key2".into(), json!({"nested": true}))
            .await;
        store.save().await.unwrap();
    }

    // Read back
    {
        let store = MemoryStore::persistent(path).unwrap();
        assert_eq!(store.get("key1").await, Some(json!("value1")));
        assert_eq!(store.get("key2").await, Some(json!({"nested": true})));
    }
}

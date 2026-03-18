use ironcrew::engine::memory::{MemoryConfig, MemoryStore};
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

#[tokio::test]
async fn test_memory_eviction_max_items() {
    let config = MemoryConfig {
        max_items: Some(3),
        max_total_tokens: None,
    };
    let store = MemoryStore::ephemeral_with_config(config);

    store.set("a".into(), json!("value_a")).await;
    store.set("b".into(), json!("value_b")).await;
    store.set("c".into(), json!("value_c")).await;
    store.set("d".into(), json!("value_d")).await; // should trigger eviction

    let keys = store.keys().await;
    assert_eq!(keys.len(), 3);
}

#[tokio::test]
async fn test_memory_eviction_preserves_accessed() {
    let config = MemoryConfig {
        max_items: Some(2),
        max_total_tokens: None,
    };
    let store = MemoryStore::ephemeral_with_config(config);

    store.set("a".into(), json!("value_a")).await;
    store.set("b".into(), json!("value_b")).await;

    // Access 'a' to increase its access_count
    store.get("a").await;
    store.get("a").await;

    store.set("c".into(), json!("value_c")).await; // should evict 'b' (less accessed)

    assert!(store.get("a").await.is_some()); // 'a' preserved (more accessed)
    assert!(store.get("c").await.is_some()); // 'c' is new
}

#[tokio::test]
async fn test_memory_token_estimation() {
    let store = MemoryStore::ephemeral();
    store.set("short".into(), json!("hi")).await;
    store
        .set(
            "long".into(),
            json!("this is a longer string with more tokens in it"),
        )
        .await;

    let stats = store.stats().await;
    assert_eq!(stats.total_items, 2);
    assert!(stats.total_tokens > 0);
}

#[tokio::test]
async fn test_memory_eviction_max_tokens() {
    let config = MemoryConfig {
        max_items: None,
        max_total_tokens: Some(10),
    };
    let store = MemoryStore::ephemeral_with_config(config);

    store.set("small".into(), json!("hi")).await; // ~1 token
    store
        .set(
            "big".into(),
            json!("this is a much longer string that has many more tokens"),
        )
        .await; // many tokens

    let stats = store.stats().await;
    // Should have evicted to stay under 10 tokens
    assert!(stats.total_tokens <= 10 || stats.total_items <= 1);
}

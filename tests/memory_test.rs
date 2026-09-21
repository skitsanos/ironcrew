use ironcrew::engine::memory::{MemoryConfig, MemoryStore};
use serde_json::json;

#[tokio::test]
async fn test_memory_set_get() {
    let store = MemoryStore::ephemeral();
    store.set("key1".into(), json!("value1")).await.unwrap();
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
    store.set("key1".into(), json!("value1")).await.unwrap();
    assert!(store.delete("key1").await);
    assert_eq!(store.get("key1").await, None);
}

#[tokio::test]
async fn test_memory_keys() {
    let store = MemoryStore::ephemeral();
    store.set("a".into(), json!(1)).await.unwrap();
    store.set("b".into(), json!(2)).await.unwrap();
    let mut keys = store.keys().await;
    keys.sort();
    assert_eq!(keys, vec!["a", "b"]);
}

#[tokio::test]
async fn test_memory_ttl_expiry() {
    let store = MemoryStore::ephemeral();
    store
        .set_with_options("temp".into(), json!("data"), vec![], Some(1))
        .await
        .unwrap();
    // Sleep a bit to let it expire
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    assert_eq!(store.get("temp").await, None);
}

#[tokio::test]
async fn test_memory_update_existing() {
    let store = MemoryStore::ephemeral();
    store.set("key".into(), json!("v1")).await.unwrap();
    store.set("key".into(), json!("v2")).await.unwrap();
    assert_eq!(store.get("key").await, Some(json!("v2")));
}

#[tokio::test]
async fn test_memory_clear() {
    let store = MemoryStore::ephemeral();
    store.set("a".into(), json!(1)).await.unwrap();
    store.set("b".into(), json!(2)).await.unwrap();
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
        .await
        .unwrap();
    store
        .set_with_options(
            "notes".into(),
            json!("Python is easy"),
            vec!["notes".into()],
            None,
        )
        .await
        .unwrap();
    let ctx = store.build_context("research findings about Rust", 5).await;
    assert!(ctx.contains("Rust is fast"));
}

#[tokio::test]
async fn test_memory_persistent_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.json");

    // Write
    {
        let store =
            MemoryStore::persistent_with_config_async(path.clone(), MemoryConfig::default())
                .await
                .unwrap();
        store.set("key1".into(), json!("value1")).await.unwrap();
        store
            .set("key2".into(), json!({"nested": true}))
            .await
            .unwrap();
        store.save().await.unwrap();
    }

    // Read back
    {
        let store = MemoryStore::persistent_with_config_async(path, MemoryConfig::default())
            .await
            .unwrap();
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

    store.set("a".into(), json!("value_a")).await.unwrap();
    store.set("b".into(), json!("value_b")).await.unwrap();
    store.set("c".into(), json!("value_c")).await.unwrap();
    store.set("d".into(), json!("value_d")).await.unwrap(); // should trigger eviction

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

    store.set("a".into(), json!("value_a")).await.unwrap();
    store.set("b".into(), json!("value_b")).await.unwrap();

    // Access 'a' to increase its access_count
    store.get("a").await;
    store.get("a").await;

    store.set("c".into(), json!("value_c")).await.unwrap(); // should evict 'b' (less accessed)

    assert!(store.get("a").await.is_some()); // 'a' preserved (more accessed)
    assert!(store.get("c").await.is_some()); // 'c' is new
}

#[tokio::test]
async fn test_memory_token_estimation() {
    let store = MemoryStore::ephemeral();
    store.set("short".into(), json!("hi")).await.unwrap();
    store
        .set(
            "long".into(),
            json!("this is a longer string with more tokens in it"),
        )
        .await
        .unwrap();

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

    store.set("small".into(), json!("hi")).await.unwrap(); // ~1 token
    store
        .set(
            "big".into(),
            json!("this is a much longer string that has many more tokens"),
        )
        .await
        .unwrap(); // many tokens

    let stats = store.stats().await;
    // Should have evicted to stay under 10 tokens
    assert!(stats.total_tokens <= 10 || stats.total_items <= 1);
}

#[test]
fn corrupt_persistent_memory_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.json");
    std::fs::write(&path, b"{ definitely-not-json").unwrap();

    let error = match MemoryStore::persistent(path) {
        Ok(_) => panic!("corrupt memory unexpectedly loaded"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid JSON"));
}

#[tokio::test]
async fn memory_input_and_context_are_bounded() {
    let store = MemoryStore::ephemeral();
    let oversized = "x".repeat(1024 * 1024 + 1);
    let error = store
        .set("too-large".into(), json!(oversized))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("MAX_VALUE_BYTES"));

    let large_but_valid = format!("needle {}", "é".repeat(200_000));
    store
        .set("bounded".into(), json!(large_but_valid))
        .await
        .unwrap();
    let context = store.build_context("needle", usize::MAX).await;
    assert!(context.len() <= 64 * 1024);
    assert!(context.is_char_boundary(context.len()));
}

#[tokio::test]
async fn concurrent_saves_leave_the_latest_in_memory_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private").join("memory.json");
    let store = MemoryStore::persistent(path.clone()).unwrap();

    let mut tasks = Vec::new();
    for index in 0..32u64 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store.set("counter".into(), json!(index)).await.unwrap();
            store.save().await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    let expected = store.get("counter").await.unwrap();
    store.save().await.unwrap();
    let reloaded = MemoryStore::persistent(path).unwrap();
    assert_eq!(reloaded.get("counter").await, Some(expected));

    let leftovers = std::fs::read_dir(dir.path().join("private"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .count();
    assert_eq!(leftovers, 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("private"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
}

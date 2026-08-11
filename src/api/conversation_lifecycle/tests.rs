use super::*;
use tokio::sync::Barrier;

fn key(index: usize) -> ConversationKey {
    ("flow".to_string(), format!("conversation-{index}"))
}

#[test]
fn sequential_high_cardinality_keys_are_removed_immediately() {
    let registry = Arc::new(ConversationLifecycleRegistry::new(4));

    for index in 0..20_000 {
        let lease = registry
            .acquire(&key(index))
            .expect("a released slot must be reusable");
        assert_eq!(registry.len(), 1);
        drop(lease);
        assert_eq!(registry.len(), 0);
    }
}

#[test]
fn capacity_bounds_distinct_keys_but_preserves_existing_key_serialization() {
    let registry = Arc::new(ConversationLifecycleRegistry::new(4));
    let leases: Vec<_> = (0..4)
        .map(|index| registry.acquire(&key(index)).expect("slot available"))
        .collect();

    assert_eq!(registry.len(), 4);
    assert!(registry.acquire(&key(4)).is_err());

    let same_key = registry
        .acquire(&key(0))
        .expect("an existing key must not consume another slot");
    assert!(Arc::ptr_eq(&same_key.gate, &leases[0].gate));
    assert_eq!(registry.len(), 4);
    drop(same_key);

    drop(leases);
    assert_eq!(registry.len(), 0);
}

#[test]
fn owned_guard_pins_the_entry_and_fails_fast_for_the_same_key() {
    let registry = Arc::new(ConversationLifecycleRegistry::new(2));
    let conversation = key(0);
    let owner = registry
        .acquire(&conversation)
        .expect("owner lease available")
        .try_lock_owned()
        .unwrap_or_else(|_| panic!("owner must acquire the gate"));

    assert_eq!(registry.len(), 1);
    let contender = registry
        .acquire(&conversation)
        .expect("same key shares its slot");
    assert!(contender.try_lock_owned().is_err());
    assert_eq!(registry.len(), 1);

    drop(owner);
    assert_eq!(registry.len(), 0);
}

#[test]
fn unrelated_keys_do_not_share_a_gate() {
    let registry = Arc::new(ConversationLifecycleRegistry::new(2));
    let first = registry.acquire(&key(0)).expect("first slot available");
    let second = registry.acquire(&key(1)).expect("second slot available");
    let _first_guard = first.try_lock().expect("first gate available");
    let _second_guard = second.try_lock().expect("second gate available");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_high_cardinality_registry_never_exceeds_capacity() {
    const CAPACITY: usize = 64;

    let registry = Arc::new(ConversationLifecycleRegistry::new(CAPACITY));
    let acquired = Arc::new(Barrier::new(CAPACITY + 1));
    let release = Arc::new(Barrier::new(CAPACITY + 1));
    let mut tasks = Vec::with_capacity(CAPACITY);

    for index in 0..CAPACITY {
        let registry = Arc::clone(&registry);
        let acquired = Arc::clone(&acquired);
        let release = Arc::clone(&release);
        tasks.push(tokio::spawn(async move {
            let _lease = registry
                .acquire(&key(index))
                .expect("one slot per concurrent key");
            acquired.wait().await;
            release.wait().await;
        }));
    }

    acquired.wait().await;
    assert_eq!(registry.len(), CAPACITY);
    assert!(registry.acquire(&key(CAPACITY)).is_err());
    release.wait().await;

    for task in tasks {
        task.await.expect("registry worker must finish");
    }
    assert_eq!(registry.len(), 0);
}

#[tokio::test]
async fn cancelling_an_owned_guard_releases_its_capacity() {
    let registry = Arc::new(ConversationLifecycleRegistry::new(1));
    let task_registry = Arc::clone(&registry);
    let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _owner = task_registry
            .acquire(&key(0))
            .expect("slot available")
            .try_lock_owned()
            .unwrap_or_else(|_| panic!("gate available"));
        acquired_tx.send(()).expect("test receiver remains open");
        std::future::pending::<()>().await;
    });

    acquired_rx.await.expect("owner acquires its gate");
    assert_eq!(registry.len(), 1);
    assert!(registry.acquire(&key(1)).is_err());

    task.abort();
    assert!(
        task.await
            .expect_err("task must be cancelled")
            .is_cancelled()
    );
    assert_eq!(registry.len(), 0);

    let replacement = registry
        .acquire(&key(1))
        .expect("cancelled owner returns its slot");
    drop(replacement);
    assert_eq!(registry.len(), 0);
}

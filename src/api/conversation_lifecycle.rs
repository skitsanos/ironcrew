use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{Mutex, OwnedMutexGuard};

pub(crate) type ConversationKey = (String, String);

const DEFAULT_MAX_ACTIVE_CONVERSATION_LIFECYCLES: usize = 256;
const HARD_MAX_ACTIVE_CONVERSATION_LIFECYCLES: usize = 4_096;

struct RegistryEntry {
    key: Arc<ConversationKey>,
    gate: Arc<Mutex<()>>,
    leases: usize,
}

/// Bounded registry of per-conversation operation gates.
///
/// Entries exist only while a caller owns a lease. Dropping the last lease
/// removes its exact key in O(1), so sequential attacker-chosen IDs neither
/// accumulate in memory nor trigger a full-map scan on later lookups.
pub struct ConversationLifecycleRegistry {
    capacity: usize,
    entries: StdMutex<HashMap<ConversationKey, RegistryEntry>>,
}

#[derive(Debug)]
pub(crate) struct ConversationLifecycleRegistryFull {
    pub(crate) capacity: usize,
}

/// Pins one registry entry for the full operation lifetime.
pub(crate) struct ConversationLifecycleLease {
    registry: Arc<ConversationLifecycleRegistry>,
    key: Arc<ConversationKey>,
    gate: Arc<Mutex<()>>,
}

/// An owned lifecycle lock used by detached message tasks. Keeping the lease
/// beside the Tokio guard prevents the registry entry from disappearing while
/// the task still holds the gate.
pub(crate) struct OwnedConversationLifecycleGuard {
    // Field order is intentional: Rust drops the mutex guard before the lease,
    // so a replacement entry cannot be published while this gate is locked.
    _guard: OwnedMutexGuard<()>,
    _lease: ConversationLifecycleLease,
}

impl ConversationLifecycleRegistry {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "lifecycle registry capacity must be positive");
        Self {
            capacity,
            entries: StdMutex::new(HashMap::with_capacity(capacity)),
        }
    }

    pub(crate) fn acquire(
        self: &Arc<Self>,
        key: &ConversationKey,
    ) -> Result<ConversationLifecycleLease, ConversationLifecycleRegistryFull> {
        let (owned_key, gate) = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = entries.get_mut(key) {
                entry.leases =
                    entry
                        .leases
                        .checked_add(1)
                        .ok_or(ConversationLifecycleRegistryFull {
                            capacity: self.capacity,
                        })?;
                (Arc::clone(&entry.key), Arc::clone(&entry.gate))
            } else {
                if entries.len() >= self.capacity {
                    return Err(ConversationLifecycleRegistryFull {
                        capacity: self.capacity,
                    });
                }
                let owned_key = Arc::new(key.clone());
                let gate = Arc::new(Mutex::new(()));
                entries.insert(
                    key.clone(),
                    RegistryEntry {
                        key: Arc::clone(&owned_key),
                        gate: Arc::clone(&gate),
                        leases: 1,
                    },
                );
                (owned_key, gate)
            }
        };

        Ok(ConversationLifecycleLease {
            registry: Arc::clone(self),
            key: owned_key,
            gate,
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl ConversationLifecycleLease {
    pub(crate) fn try_lock(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, tokio::sync::TryLockError> {
        self.gate.try_lock()
    }

    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }

    pub(crate) fn try_lock_owned(self) -> Result<OwnedConversationLifecycleGuard, Self> {
        match Arc::clone(&self.gate).try_lock_owned() {
            Ok(guard) => Ok(OwnedConversationLifecycleGuard {
                _guard: guard,
                _lease: self,
            }),
            Err(_) => Err(self),
        }
    }
}

impl Drop for ConversationLifecycleLease {
    fn drop(&mut self) {
        let mut entries = self
            .registry
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = entries.get_mut(self.key.as_ref()).is_some_and(|entry| {
            if !Arc::ptr_eq(&entry.gate, &self.gate) {
                return false;
            }
            debug_assert!(entry.leases > 0);
            entry.leases = entry.leases.saturating_sub(1);
            entry.leases == 0
        });
        if remove {
            entries.remove(self.key.as_ref());
        }
    }
}

/// Maximum number of distinct conversation lifecycle keys held concurrently
/// by one server process.
pub fn max_active_conversation_lifecycles() -> usize {
    positive_bounded_env(
        "IRONCREW_MAX_CONVERSATION_LIFECYCLES",
        DEFAULT_MAX_ACTIVE_CONVERSATION_LIFECYCLES,
        HARD_MAX_ACTIVE_CONVERSATION_LIFECYCLES,
    )
}

fn positive_bounded_env(name: &str, default: usize, upper: usize) -> usize {
    let fallback = default.min(upper);
    match std::env::var(name) {
        Ok(raw) => match raw.parse::<usize>() {
            Ok(value) if value > 0 => value.min(upper),
            _ => {
                tracing::warn!(
                    variable = name,
                    value = %raw,
                    default = fallback,
                    "Ignoring invalid resource-limit environment value"
                );
                fallback
            }
        },
        Err(_) => fallback,
    }
}

#[cfg(test)]
mod tests;

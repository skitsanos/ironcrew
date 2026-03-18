use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::utils::error::{IronCrewError, Result};

/// Configuration for memory store limits.
#[derive(Debug, Clone, Default)]
pub struct MemoryConfig {
    pub max_items: Option<usize>,
    pub max_total_tokens: Option<usize>,
}

/// Rough token estimate: ~4 chars per token for English text.
fn estimate_tokens(value: &serde_json::Value) -> usize {
    let text = match value {
        serde_json::Value::String(s) => s.len(),
        other => serde_json::to_string(other).unwrap_or_default().len(),
    };
    text.div_ceil(4)
}

/// A single memory item with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub key: String,
    pub value: serde_json::Value,
    pub created_at: i64,
    pub updated_at: i64,
    pub access_count: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub ttl_ms: Option<i64>,
    #[serde(default)]
    pub estimated_tokens: usize,
}

impl MemoryItem {
    pub fn new(key: String, value: serde_json::Value) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let estimated_tokens = estimate_tokens(&value);
        Self {
            key,
            value,
            created_at: now,
            updated_at: now,
            access_count: 0,
            tags: Vec::new(),
            ttl_ms: None,
            estimated_tokens,
        }
    }

    pub fn is_expired(&self) -> bool {
        if let Some(ttl) = self.ttl_ms {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            now > self.created_at + ttl
        } else {
            false
        }
    }
}

/// Memory statistics for inspection.
#[derive(Debug, Clone)]
pub struct MemoryStats {
    pub total_items: usize,
    pub total_tokens: usize,
    #[allow(dead_code)] // part of public API
    pub max_items: Option<usize>,
    #[allow(dead_code)] // part of public API
    pub max_total_tokens: Option<usize>,
}

/// Thread-safe memory store with pluggable backend.
#[derive(Clone)]
pub struct MemoryStore {
    items: Arc<RwLock<HashMap<String, MemoryItem>>>,
    backend: MemoryBackend,
    config: MemoryConfig,
}

#[derive(Clone)]
pub enum MemoryBackend {
    /// In-memory only, lost when the process exits
    Ephemeral,
    /// Persisted to a JSON file
    Persistent { path: PathBuf },
}

impl MemoryStore {
    #[allow(dead_code)] // used in integration tests
    pub fn ephemeral() -> Self {
        Self::with_config(MemoryBackend::Ephemeral, MemoryConfig::default())
    }

    pub fn ephemeral_with_config(config: MemoryConfig) -> Self {
        Self::with_config(MemoryBackend::Ephemeral, config)
    }

    #[allow(dead_code)] // used in integration tests
    pub fn persistent(path: PathBuf) -> Result<Self> {
        Self::persistent_with_config(path, MemoryConfig::default())
    }

    pub fn persistent_with_config(path: PathBuf, config: MemoryConfig) -> Result<Self> {
        let items = if path.exists() {
            let data = std::fs::read_to_string(&path).map_err(IronCrewError::Io)?;
            let items: HashMap<String, MemoryItem> =
                serde_json::from_str(&data).unwrap_or_default();
            // Filter out expired items on load
            items.into_iter().filter(|(_, v)| !v.is_expired()).collect()
        } else {
            HashMap::new()
        };

        Ok(Self::with_config_and_items(
            MemoryBackend::Persistent { path },
            config,
            items,
        ))
    }

    fn with_config(backend: MemoryBackend, config: MemoryConfig) -> Self {
        Self {
            items: Arc::new(RwLock::new(HashMap::new())),
            backend,
            config,
        }
    }

    fn with_config_and_items(
        backend: MemoryBackend,
        config: MemoryConfig,
        items: HashMap<String, MemoryItem>,
    ) -> Self {
        Self {
            items: Arc::new(RwLock::new(items)),
            backend,
            config,
        }
    }

    /// Set a value in memory.
    pub async fn set(&self, key: String, value: serde_json::Value) {
        {
            let mut items = self.items.write().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            if let Some(existing) = items.get_mut(&key) {
                existing.estimated_tokens = estimate_tokens(&value);
                existing.value = value;
                existing.updated_at = now;
            } else {
                items.insert(key.clone(), MemoryItem::new(key, value));
            }
        }
        self.evict_if_needed().await;
    }

    /// Set a value with tags and optional TTL.
    pub async fn set_with_options(
        &self,
        key: String,
        value: serde_json::Value,
        tags: Vec<String>,
        ttl_ms: Option<i64>,
    ) {
        {
            let mut items = self.items.write().await;
            let mut item = MemoryItem::new(key.clone(), value);
            item.tags = tags;
            item.ttl_ms = ttl_ms;
            items.insert(key, item);
        }
        self.evict_if_needed().await;
    }

    /// Get a value from memory. Returns None if not found or expired.
    pub async fn get(&self, key: &str) -> Option<serde_json::Value> {
        let mut items = self.items.write().await;
        if let Some(item) = items.get_mut(key) {
            if item.is_expired() {
                items.remove(key);
                return None;
            }
            item.access_count += 1;
            Some(item.value.clone())
        } else {
            None
        }
    }

    /// Delete a key from memory.
    pub async fn delete(&self, key: &str) -> bool {
        let mut items = self.items.write().await;
        items.remove(key).is_some()
    }

    /// List all keys in memory (excluding expired).
    pub async fn keys(&self) -> Vec<String> {
        let items = self.items.read().await;
        items
            .iter()
            .filter(|(_, v)| !v.is_expired())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Build a context string from memory items relevant to a query.
    /// Uses simple keyword matching for relevance scoring.
    pub async fn build_context(&self, query: &str, max_items: usize) -> String {
        let items = self.items.read().await;
        let query_words: std::collections::HashSet<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(|s| {
                s.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect();

        let mut scored: Vec<(&MemoryItem, f32)> = items
            .values()
            .filter(|item| !item.is_expired())
            .map(|item| {
                let mut score = 0.0f32;

                // Tag match: +3 per matching tag
                for tag in &item.tags {
                    if query_words.contains(&tag.to_lowercase()) {
                        score += 3.0;
                    }
                }

                // Key match: +2 if query contains the key
                if query_words.contains(&item.key.to_lowercase()) {
                    score += 2.0;
                }

                // Value content match: +1 per overlapping word
                if let Some(s) = item.value.as_str() {
                    let value_words: std::collections::HashSet<String> = s
                        .to_lowercase()
                        .split_whitespace()
                        .map(|w| {
                            w.trim_matches(|c: char| !c.is_alphanumeric())
                                .to_string()
                        })
                        .collect();
                    let overlap = query_words.intersection(&value_words).count();
                    score += overlap as f32;
                }

                // Recency bonus: more recent = higher score
                score += 1.0 / (1.0 + (item.access_count as f32).ln());

                (item, score)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        scored
            .iter()
            .take(max_items)
            .map(|(item, _)| {
                let value_str = match &item.value {
                    serde_json::Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                format!("[{}]: {}", item.key, value_str)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Persist to disk (only for Persistent backend).
    pub async fn save(&self) -> Result<()> {
        if let MemoryBackend::Persistent { ref path } = self.backend {
            let items = self.items.read().await;
            // Filter expired before saving
            let active: HashMap<&String, &MemoryItem> =
                items.iter().filter(|(_, v)| !v.is_expired()).collect();
            let json = serde_json::to_string_pretty(&active).map_err(|e| {
                IronCrewError::Validation(format!("Failed to serialize memory: {}", e))
            })?;

            // Ensure parent directory exists
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            std::fs::write(path, json)?;
            tracing::debug!("Memory persisted to {}", path.display());
        }
        Ok(())
    }

    /// Clear all memory.
    pub async fn clear(&self) {
        let mut items = self.items.write().await;
        items.clear();
    }

    /// Get memory statistics.
    pub async fn stats(&self) -> MemoryStats {
        let items = self.items.read().await;
        let active: Vec<&MemoryItem> = items.values().filter(|v| !v.is_expired()).collect();
        MemoryStats {
            total_items: active.len(),
            total_tokens: active.iter().map(|v| v.estimated_tokens).sum(),
            max_items: self.config.max_items,
            max_total_tokens: self.config.max_total_tokens,
        }
    }

    /// Evict items to stay within configured limits.
    /// Removes least-recently-accessed (by access_count, then oldest updated_at) items first.
    async fn evict_if_needed(&self) {
        if self.config.max_items.is_none() && self.config.max_total_tokens.is_none() {
            return; // No limits configured, nothing to evict
        }

        let mut items = self.items.write().await;

        // Remove expired items first
        items.retain(|_, v| !v.is_expired());

        // Check max_items limit
        if let Some(max) = self.config.max_items {
            while items.len() > max {
                if let Some(key) = Self::find_eviction_candidate(&items) {
                    tracing::debug!("Evicting memory item '{}' (max_items exceeded)", key);
                    items.remove(&key);
                } else {
                    break;
                }
            }
        }

        // Check max_total_tokens limit
        if let Some(max_tokens) = self.config.max_total_tokens {
            let mut total: usize = items.values().map(|v| v.estimated_tokens).sum();
            while total > max_tokens && !items.is_empty() {
                if let Some(key) = Self::find_eviction_candidate(&items) {
                    if let Some(removed) = items.remove(&key) {
                        tracing::debug!(
                            "Evicting memory item '{}' (max_tokens exceeded, {} tokens)",
                            key,
                            removed.estimated_tokens
                        );
                        total -= removed.estimated_tokens;
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Find the best candidate for eviction: least accessed, then oldest.
    fn find_eviction_candidate(items: &HashMap<String, MemoryItem>) -> Option<String> {
        items
            .iter()
            .min_by(|a, b| {
                a.1.access_count
                    .cmp(&b.1.access_count)
                    .then(a.1.updated_at.cmp(&b.1.updated_at))
            })
            .map(|(k, _)| k.clone())
    }
}

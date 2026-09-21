use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, RwLock};

use crate::utils::error::{IronCrewError, Result};

#[path = "memory/persistence.rs"]
mod persistence;

/// Configuration for memory store limits.
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    pub max_items: Option<usize>,
    pub max_total_tokens: Option<usize>,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_items: Some(500),
            max_total_tokens: Some(50_000),
        }
    }
}

/// Rough token estimate: ~4 chars per token for English text.
fn estimate_tokens(value: &serde_json::Value) -> usize {
    let text = match value {
        serde_json::Value::String(s) => s.len(),
        other => serde_json::to_string(other).unwrap_or_default().len(),
    };
    text.div_ceil(4)
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
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
    #[serde(default)]
    pub revision: u64,
}

impl MemoryItem {
    pub fn new(key: String, value: serde_json::Value, revision: u64) -> Self {
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
            revision,
        }
    }

    pub fn is_expired(&self) -> bool {
        if let Some(ttl) = self.ttl_ms {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            now > self.created_at.saturating_add(ttl)
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
    next_revision: Arc<AtomicU64>,
    /// Serializes persistent snapshots so an older save can never replace a
    /// newer snapshot after racing it to disk.
    save_lock: Arc<Mutex<()>>,
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
        let config = Self::bounded_config(config);
        let mut items = Self::load_persistent_items(&path)?;
        Self::evict_items(&config, &mut items);

        Ok(Self::with_config_and_items(
            MemoryBackend::Persistent { path },
            config,
            items,
        ))
    }

    fn with_config(backend: MemoryBackend, config: MemoryConfig) -> Self {
        let config = Self::bounded_config(config);
        Self {
            items: Arc::new(RwLock::new(HashMap::new())),
            backend,
            config,
            next_revision: Arc::new(AtomicU64::new(1)),
            save_lock: Arc::new(Mutex::new(())),
        }
    }

    fn with_config_and_items(
        backend: MemoryBackend,
        config: MemoryConfig,
        items: HashMap<String, MemoryItem>,
    ) -> Self {
        let config = Self::bounded_config(config);
        let next_revision = items
            .values()
            .map(|item| item.revision)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Self {
            items: Arc::new(RwLock::new(items)),
            backend,
            config,
            next_revision: Arc::new(AtomicU64::new(next_revision)),
            save_lock: Arc::new(Mutex::new(())),
        }
    }

    fn env_limit(name: &str, default: usize, hard_max: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .map(|value| value.min(hard_max))
            .unwrap_or(default)
    }

    fn bounded_config(mut config: MemoryConfig) -> MemoryConfig {
        const HARD_ITEMS: usize = 100_000;
        const HARD_TOKENS: usize = 10_000_000;

        config.max_items = config.max_items.map(|value| value.min(HARD_ITEMS));
        config.max_total_tokens = config.max_total_tokens.map(|value| value.min(HARD_TOKENS));
        if config.max_items.is_none() && config.max_total_tokens.is_none() {
            // A process-level store must retain at least one finite eviction
            // dimension even when constructed directly from Rust rather than
            // through the validated Lua Crew.new surface.
            config = MemoryConfig::default();
        }
        config
    }

    fn max_key_bytes() -> usize {
        Self::env_limit("IRONCREW_MEMORY_MAX_KEY_BYTES", 1024, 16 * 1024)
    }

    fn max_value_bytes() -> usize {
        Self::env_limit(
            "IRONCREW_MEMORY_MAX_VALUE_BYTES",
            1024 * 1024,
            16 * 1024 * 1024,
        )
    }

    fn max_tags() -> usize {
        Self::env_limit("IRONCREW_MEMORY_MAX_TAGS", 32, 256)
    }

    fn max_tag_bytes() -> usize {
        Self::env_limit("IRONCREW_MEMORY_MAX_TAG_BYTES", 256, 4 * 1024)
    }

    fn max_persistent_bytes() -> usize {
        Self::env_limit(
            "IRONCREW_MEMORY_PERSIST_MAX_BYTES",
            16 * 1024 * 1024,
            64 * 1024 * 1024,
        )
    }

    fn validate_item_input(
        key: &str,
        value: &serde_json::Value,
        tags: &[String],
        ttl_ms: Option<i64>,
    ) -> Result<()> {
        if key.is_empty() {
            return Err(IronCrewError::Validation(
                "memory key must not be empty".into(),
            ));
        }
        if key.len() > Self::max_key_bytes() {
            return Err(IronCrewError::Validation(format!(
                "memory key is {} bytes, exceeds IRONCREW_MEMORY_MAX_KEY_BYTES ({})",
                key.len(),
                Self::max_key_bytes()
            )));
        }

        let value_bytes = serde_json::to_vec(value).map_err(|error| {
            IronCrewError::Validation(format!("failed to serialize memory value: {error}"))
        })?;
        if value_bytes.len() > Self::max_value_bytes() {
            return Err(IronCrewError::Validation(format!(
                "memory value is {} bytes, exceeds IRONCREW_MEMORY_MAX_VALUE_BYTES ({})",
                value_bytes.len(),
                Self::max_value_bytes()
            )));
        }
        if tags.len() > Self::max_tags() {
            return Err(IronCrewError::Validation(format!(
                "memory item has {} tags, exceeds IRONCREW_MEMORY_MAX_TAGS ({})",
                tags.len(),
                Self::max_tags()
            )));
        }
        if let Some(tag) = tags.iter().find(|tag| tag.len() > Self::max_tag_bytes()) {
            return Err(IronCrewError::Validation(format!(
                "memory tag is {} bytes, exceeds IRONCREW_MEMORY_MAX_TAG_BYTES ({})",
                tag.len(),
                Self::max_tag_bytes()
            )));
        }
        if ttl_ms.is_some_and(|ttl| ttl <= 0) {
            return Err(IronCrewError::Validation(
                "memory ttl_ms must be greater than zero".into(),
            ));
        }
        Ok(())
    }

    fn load_persistent_items(path: &std::path::Path) -> Result<HashMap<String, MemoryItem>> {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => return Err(IronCrewError::Io(error)),
        };
        let metadata = file.metadata().map_err(IronCrewError::Io)?;
        if !metadata.is_file() {
            return Err(IronCrewError::Validation(format!(
                "memory path '{}' is not a regular file",
                path.display()
            )));
        }
        let max_bytes = Self::max_persistent_bytes();
        if metadata.len() > max_bytes as u64 {
            return Err(IronCrewError::Validation(format!(
                "memory file is {} bytes, exceeds IRONCREW_MEMORY_PERSIST_MAX_BYTES ({max_bytes})",
                metadata.len()
            )));
        }

        let mut data = Vec::with_capacity((metadata.len() as usize).min(max_bytes));
        std::io::Read::by_ref(&mut file)
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut data)
            .map_err(IronCrewError::Io)?;
        if data.len() > max_bytes {
            return Err(IronCrewError::Validation(format!(
                "memory file grew beyond IRONCREW_MEMORY_PERSIST_MAX_BYTES ({max_bytes}) while reading"
            )));
        }

        let items: HashMap<String, MemoryItem> =
            serde_json::from_slice(&data).map_err(|error| {
                IronCrewError::Validation(format!(
                    "memory file '{}' contains invalid JSON: {error}",
                    path.display()
                ))
            })?;
        let mut active = HashMap::with_capacity(items.len());
        for (stored_key, item) in items {
            if stored_key != item.key {
                return Err(IronCrewError::Validation(format!(
                    "memory file '{}' has a mismatched item key",
                    path.display()
                )));
            }
            Self::validate_item_input(&stored_key, &item.value, &item.tags, item.ttl_ms)?;
            if !item.is_expired() {
                active.insert(stored_key, item);
            }
        }
        Ok(active)
    }

    fn allocate_revision(&self) -> u64 {
        self.next_revision
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(1))
            })
            .unwrap_or(u64::MAX)
    }

    /// Set a value in memory.
    pub async fn set(&self, key: String, value: serde_json::Value) -> Result<()> {
        Self::validate_item_input(&key, &value, &[], None)?;
        let revision = self.allocate_revision();
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
                existing.revision = revision;
            } else {
                items.insert(key.clone(), MemoryItem::new(key, value, revision));
            }
            Self::evict_items(&self.config, &mut items);
        }
        Ok(())
    }

    /// Set a value with tags and optional TTL.
    pub async fn set_with_options(
        &self,
        key: String,
        value: serde_json::Value,
        tags: Vec<String>,
        ttl_ms: Option<i64>,
    ) -> Result<()> {
        Self::validate_item_input(&key, &value, &tags, ttl_ms)?;
        let revision = self.allocate_revision();
        {
            let mut items = self.items.write().await;
            let mut item = MemoryItem::new(key.clone(), value, revision);
            item.tags = tags;
            item.ttl_ms = ttl_ms;
            items.insert(key, item);
            Self::evict_items(&self.config, &mut items);
        }
        Ok(())
    }

    /// Get a value from memory. Returns None if not found or expired.
    pub async fn get(&self, key: &str) -> Option<serde_json::Value> {
        let revision = self.allocate_revision();
        let mut items = self.items.write().await;
        if let Some(item) = items.get_mut(key) {
            if item.is_expired() {
                items.remove(key);
                return None;
            }
            item.access_count = item.access_count.saturating_add(1);
            item.revision = revision;
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
        let max_context_bytes =
            Self::env_limit("IRONCREW_MEMORY_CONTEXT_MAX_BYTES", 64 * 1024, 1024 * 1024);
        let max_query_bytes =
            Self::env_limit("IRONCREW_MEMORY_QUERY_MAX_BYTES", 16 * 1024, 256 * 1024);
        let query = utf8_prefix(query, max_query_bytes);
        let items = self.items.read().await;
        let query_words: std::collections::HashSet<String> = query
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
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
                    let value_words: std::collections::HashSet<String> =
                        utf8_prefix(s, max_context_bytes)
                            .to_lowercase()
                            .split_whitespace()
                            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
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

        let mut context = String::new();
        for (item, _) in scored.iter().take(max_items) {
            let value = match &item.value {
                serde_json::Value::String(value) => value.as_str().into(),
                other => std::borrow::Cow::Owned(serde_json::to_string(other).unwrap_or_default()),
            };
            let prefix = format!("[{}]: ", item.key);
            let separator_bytes = usize::from(!context.is_empty());
            let Some(remaining) =
                max_context_bytes.checked_sub(context.len() + prefix.len() + separator_bytes)
            else {
                break;
            };
            if !context.is_empty() {
                context.push('\n');
            }
            context.push_str(&prefix);
            context.push_str(utf8_prefix(&value, remaining));
            if value.len() > remaining {
                break;
            }
        }
        context
    }

    /// Persist to disk (only for Persistent backend).
    pub async fn save(&self) -> Result<()> {
        if let MemoryBackend::Persistent { ref path } = self.backend {
            let _save_guard = self.save_lock.lock().await;
            let json = {
                let items = self.items.read().await;
                let active: HashMap<&String, &MemoryItem> = items
                    .iter()
                    .filter(|(_, value)| !value.is_expired())
                    .collect();
                crate::utils::http::to_json_pretty_limited(
                    &active,
                    Self::max_persistent_bytes(),
                )
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "failed to serialize memory within IRONCREW_MEMORY_PERSIST_MAX_BYTES ({}): {error}",
                        Self::max_persistent_bytes()
                    ))
                })?
            };

            let save_path = path.clone();
            tokio::task::spawn_blocking(move || Self::atomic_save(&save_path, json.as_bytes()))
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "memory snapshot blocking task failed while saving: {error}"
                    ))
                })??;
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
        let (total_items, total_tokens) = items.values().filter(|value| !value.is_expired()).fold(
            (0usize, 0usize),
            |(count, tokens), item| {
                (
                    count.saturating_add(1),
                    tokens.saturating_add(item.estimated_tokens),
                )
            },
        );
        MemoryStats {
            total_items,
            total_tokens,
            max_items: self.config.max_items,
            max_total_tokens: self.config.max_total_tokens,
        }
    }

    /// Evict items to stay within configured limits.
    /// Removes least-recently-accessed (by access_count, then oldest updated_at) items first.
    fn evict_items(config: &MemoryConfig, items: &mut HashMap<String, MemoryItem>) {
        if config.max_items.is_none() && config.max_total_tokens.is_none() {
            return; // No limits configured, nothing to evict
        }

        // Remove expired items first
        items.retain(|_, v| !v.is_expired());

        // Check max_items limit
        if let Some(max) = config.max_items {
            while items.len() > max {
                if let Some(key) = Self::find_eviction_candidate(items) {
                    tracing::debug!("Evicting memory item '{}' (max_items exceeded)", key);
                    items.remove(&key);
                } else {
                    break;
                }
            }
        }

        // Check max_total_tokens limit
        if let Some(max_tokens) = config.max_total_tokens {
            let mut total = items.values().fold(0usize, |sum, item| {
                sum.saturating_add(item.estimated_tokens)
            });
            while total > max_tokens && !items.is_empty() {
                if let Some(key) = Self::find_eviction_candidate(items) {
                    if let Some(removed) = items.remove(&key) {
                        tracing::debug!(
                            "Evicting memory item '{}' (max_tokens exceeded, {} tokens)",
                            key,
                            removed.estimated_tokens
                        );
                        total = total.saturating_sub(removed.estimated_tokens);
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// Find the best candidate for eviction: least accessed, then least recently touched.
    fn find_eviction_candidate(items: &HashMap<String, MemoryItem>) -> Option<String> {
        items
            .iter()
            .min_by(|a, b| {
                a.1.access_count
                    .cmp(&b.1.access_count)
                    .then(a.1.updated_at.cmp(&b.1.updated_at))
                    .then(a.1.revision.cmp(&b.1.revision))
            })
            .map(|(k, _)| k.clone())
    }
}

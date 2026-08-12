use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

const DEFAULT_MESSAGE_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_QUEUE_MAX_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_HISTORY_MAX_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_PENDING_MAX_BYTES: usize = 4 * 1024 * 1024;
const HARD_MESSAGE_MAX_BYTES: usize = 4 * 1024 * 1024;
const HARD_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;
const HARD_HISTORY_MAX_BYTES: usize = 64 * 1024 * 1024;
const HARD_PENDING_MAX_BYTES: usize = 64 * 1024 * 1024;
const HARD_QUEUE_DEPTH: usize = 10_000;
const HARD_HISTORY_DEPTH: usize = 10_000;
const HARD_PENDING_DEPTH: usize = 5_000;

fn positive_env_limit(name: &str, default: usize, hard_max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(hard_max))
        .unwrap_or(default)
}

fn message_size(message: &Message) -> usize {
    message
        .content
        .len()
        .saturating_add(message.from.len())
        .saturating_add(message.to.len())
        .saturating_add(message.id.len())
        .saturating_add(message.reply_to.as_ref().map_or(0, String::len))
        .saturating_add(64)
}

fn truncate_message_content(content: &mut String, max_bytes: usize) -> bool {
    if content.len() <= max_bytes {
        return false;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !content.is_char_boundary(boundary) {
        boundary -= 1;
    }
    content.truncate(boundary);
    content.shrink_to_fit();
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageType {
    Notification,
    Request,
    Response,
    Broadcast,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String, // agent name, or "*" for broadcast
    pub content: String,
    pub message_type: MessageType,
    pub timestamp: i64,
    pub reply_to: Option<String>, // id of the message this replies to
}

impl Message {
    pub fn new(from: String, to: String, content: String, message_type: MessageType) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            from,
            to,
            content,
            message_type,
            timestamp: now,
            reply_to: None,
        }
    }

    #[allow(dead_code)] // used in integration tests
    pub fn reply(original: &Message, from: String, content: String) -> Self {
        let mut msg = Message::new(from, original.from.clone(), content, MessageType::Response);
        msg.reply_to = Some(original.id.clone());
        msg
    }
}

/// Thread-safe message bus for agent-to-agent communication.
#[derive(Clone)]
pub struct MessageBus {
    /// Queued messages per agent name. Messages are consumed when delivered.
    queues: Arc<RwLock<HashMap<String, VecDeque<Arc<Message>>>>>,
    /// History of all messages (for debugging/inspection), capped.
    history: Arc<RwLock<VecDeque<Arc<Message>>>>,
    /// Pending broadcasts sent before agents were registered.
    pending_broadcasts: Arc<RwLock<Vec<Arc<Message>>>>,
}

/// Returns the max per-agent queue depth from the environment,
/// defaulting to 1000 messages.
fn queue_depth_limit() -> usize {
    positive_env_limit("IRONCREW_MESSAGEBUS_QUEUE_DEPTH", 1_000, HARD_QUEUE_DEPTH)
}

/// Returns the max pending-broadcasts cap from the environment,
/// defaulting to 500.
fn pending_cap_limit() -> usize {
    positive_env_limit("IRONCREW_MESSAGEBUS_PENDING_CAP", 500, HARD_PENDING_DEPTH)
}

/// Drop oldest messages from a single queue until it's under `cap`.
/// Logs a warning on each eviction so operators can see the pressure.
fn enforce_queue_cap(queue: &mut VecDeque<Arc<Message>>, agent_name: &str, cap: usize) {
    let byte_cap = positive_env_limit(
        "IRONCREW_MESSAGEBUS_QUEUE_MAX_BYTES",
        DEFAULT_QUEUE_MAX_BYTES,
        HARD_QUEUE_MAX_BYTES,
    );
    while queue.len() > cap
        || queue.iter().fold(0usize, |total, message| {
            total.saturating_add(message_size(message))
        }) > byte_cap
    {
        queue.pop_front();
        tracing::warn!(
            "MessageBus: queue for '{}' exceeded its count/byte budget, dropping oldest message",
            agent_name,
        );
    }
}

impl MessageBus {
    pub fn new() -> Self {
        Self {
            queues: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(VecDeque::with_capacity(500))),
            pending_broadcasts: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Clear pending broadcasts (call after all agents are registered).
    pub async fn clear_pending_broadcasts(&self) {
        self.pending_broadcasts.write().await.clear();
    }

    /// Send a message to a specific agent or broadcast to all.
    pub async fn send(&self, mut message: Message) {
        let content_limit = positive_env_limit(
            "IRONCREW_MESSAGEBUS_MESSAGE_MAX_BYTES",
            DEFAULT_MESSAGE_MAX_BYTES,
            HARD_MESSAGE_MAX_BYTES,
        );
        if truncate_message_content(&mut message.content, content_limit) {
            tracing::warn!(
                message_id = %message.id,
                content_limit,
                "MessageBus message content was truncated"
            );
        }
        let message = Arc::new(message);
        let depth_cap = queue_depth_limit();

        let mut history = self.history.write().await;
        history.push_back(Arc::clone(&message));
        let history_depth =
            positive_env_limit("IRONCREW_MESSAGEBUS_HISTORY_DEPTH", 500, HARD_HISTORY_DEPTH);
        let history_bytes = positive_env_limit(
            "IRONCREW_MESSAGEBUS_HISTORY_MAX_BYTES",
            DEFAULT_HISTORY_MAX_BYTES,
            HARD_HISTORY_MAX_BYTES,
        );
        while history.len() > history_depth
            || history.iter().fold(0usize, |total, item| {
                total.saturating_add(message_size(item))
            }) > history_bytes
        {
            history.pop_front();
        }
        drop(history);

        let mut queues = self.queues.write().await;

        if message.to == "*" {
            // Broadcast: add to all existing queues except sender (zero-copy via Arc)
            let agent_names: Vec<String> = queues.keys().cloned().collect();
            if agent_names.is_empty() {
                // No agents registered yet — store for later delivery,
                // respecting the pending-cap.
                drop(queues);
                let pending_cap = pending_cap_limit();
                let mut pending = self.pending_broadcasts.write().await;
                pending.push(message);
                while pending.len() > pending_cap {
                    pending.remove(0); // drop oldest
                    tracing::warn!(
                        "MessageBus: pending_broadcasts cap ({}) exceeded, dropping oldest",
                        pending_cap
                    );
                }
                let pending_byte_cap = positive_env_limit(
                    "IRONCREW_MESSAGEBUS_PENDING_MAX_BYTES",
                    DEFAULT_PENDING_MAX_BYTES,
                    HARD_PENDING_MAX_BYTES,
                );
                while pending.iter().fold(0usize, |total, item| {
                    total.saturating_add(message_size(item))
                }) > pending_byte_cap
                {
                    pending.remove(0);
                    tracing::warn!(
                        "MessageBus: pending broadcasts exceeded byte cap ({}), dropping oldest",
                        pending_byte_cap
                    );
                }
                return;
            }
            for name in agent_names {
                if name != message.from {
                    let queue = queues.entry(name.clone()).or_default();
                    queue.push_back(Arc::clone(&message));
                    enforce_queue_cap(queue, &name, depth_cap);
                }
            }
        } else {
            let target = message.to.clone();
            let queue = queues.entry(target.clone()).or_default();
            queue.push_back(message);
            enforce_queue_cap(queue, &target, depth_cap);
        }
    }

    /// Register an agent (creates their message queue and delivers pending broadcasts).
    pub async fn register_agent(&self, name: &str) {
        let mut queues = self.queues.write().await;
        queues.entry(name.to_string()).or_default();

        // Deliver any pending broadcasts to this agent (zero-copy via Arc)
        let pending = self.pending_broadcasts.read().await;
        let depth_cap = queue_depth_limit();
        for msg in pending.iter() {
            if msg.from != name {
                let queue = queues.entry(name.to_string()).or_default();
                queue.push_back(Arc::clone(msg));
                enforce_queue_cap(queue, name, depth_cap);
            }
        }
    }

    /// Retrieve and consume all pending messages for an agent.
    pub async fn receive(&self, agent_name: &str) -> Vec<Arc<Message>> {
        let mut queues = self.queues.write().await;
        queues
            .get_mut(agent_name)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Peek at pending messages without consuming them.
    #[allow(dead_code)] // used in integration tests
    pub async fn peek(&self, agent_name: &str) -> Vec<Arc<Message>> {
        let queues = self.queues.read().await;
        queues
            .get(agent_name)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get count of pending messages for an agent.
    #[allow(dead_code)] // used in integration tests
    pub async fn pending_count(&self, agent_name: &str) -> usize {
        let queues = self.queues.read().await;
        queues.get(agent_name).map(|q| q.len()).unwrap_or(0)
    }

    /// Get full message history.
    pub async fn get_history(&self) -> Vec<Arc<Message>> {
        let history = self.history.read().await;
        history.iter().cloned().collect()
    }
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod resource_limit_tests {
    use super::*;

    #[test]
    fn message_truncation_preserves_utf8_boundary() {
        let mut content = "🦀🦀🦀".to_string();
        assert!(truncate_message_content(&mut content, 5));
        assert_eq!(content, "🦀");
        assert!(std::str::from_utf8(content.as_bytes()).is_ok());
    }

    #[test]
    fn message_size_is_saturating_and_includes_payload() {
        let message = Message::new(
            "a".into(),
            "b".into(),
            "payload".into(),
            MessageType::Notification,
        );
        assert!(message_size(&message) >= message.content.len());
    }
}

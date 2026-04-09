use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

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
/// defaulting to 1000 messages. A value of 0 disables the cap.
fn queue_depth_limit() -> Option<usize> {
    match std::env::var("IRONCREW_MESSAGEBUS_QUEUE_DEPTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(1000),
    }
}

/// Returns the max pending-broadcasts cap from the environment,
/// defaulting to 500. A value of 0 disables the cap.
fn pending_cap_limit() -> Option<usize> {
    match std::env::var("IRONCREW_MESSAGEBUS_PENDING_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(500),
    }
}

/// Drop oldest messages from a single queue until it's under `cap`.
/// Logs a warning on each eviction so operators can see the pressure.
fn enforce_queue_cap(queue: &mut VecDeque<Arc<Message>>, agent_name: &str, cap: Option<usize>) {
    let Some(cap) = cap else {
        return;
    };
    while queue.len() > cap {
        queue.pop_front();
        tracing::warn!(
            "MessageBus: queue for '{}' exceeded depth cap ({}), dropping oldest message",
            agent_name,
            cap
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
    pub async fn send(&self, message: Message) {
        let message = Arc::new(message);
        let depth_cap = queue_depth_limit();

        let mut history = self.history.write().await;
        history.push_back(Arc::clone(&message));
        // Cap history to last 500 messages (O(1) with VecDeque)
        while history.len() > 500 {
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
                if let Some(cap) = pending_cap {
                    while pending.len() > cap {
                        pending.remove(0); // drop oldest
                        tracing::warn!(
                            "MessageBus: pending_broadcasts cap ({}) exceeded, dropping oldest",
                            cap
                        );
                    }
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

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

    #[allow(dead_code)]
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
    queues: Arc<RwLock<HashMap<String, VecDeque<Message>>>>,
    /// History of all messages (for debugging/inspection).
    history: Arc<RwLock<Vec<Message>>>,
    /// Pending broadcasts sent before agents were registered.
    pending_broadcasts: Arc<RwLock<Vec<Message>>>,
}

impl MessageBus {
    pub fn new() -> Self {
        Self {
            queues: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(Vec::new())),
            pending_broadcasts: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Send a message to a specific agent or broadcast to all.
    pub async fn send(&self, message: Message) {
        let mut history = self.history.write().await;
        history.push(message.clone());

        let mut queues = self.queues.write().await;

        if message.to == "*" {
            // Broadcast: add to all existing queues except sender
            let agent_names: Vec<String> = queues.keys().cloned().collect();
            if agent_names.is_empty() {
                // No agents registered yet — store for later delivery
                drop(queues);
                self.pending_broadcasts.write().await.push(message);
                return;
            }
            for name in agent_names {
                if name != message.from {
                    queues.entry(name).or_default().push_back(message.clone());
                }
            }
        } else {
            queues
                .entry(message.to.clone())
                .or_default()
                .push_back(message);
        }
    }

    /// Register an agent (creates their message queue and delivers pending broadcasts).
    pub async fn register_agent(&self, name: &str) {
        let mut queues = self.queues.write().await;
        queues.entry(name.to_string()).or_default();

        // Deliver any pending broadcasts to this agent
        let pending = self.pending_broadcasts.read().await;
        for msg in pending.iter() {
            if msg.from != name {
                queues
                    .entry(name.to_string())
                    .or_default()
                    .push_back(msg.clone());
            }
        }
    }

    /// Retrieve and consume all pending messages for an agent.
    pub async fn receive(&self, agent_name: &str) -> Vec<Message> {
        let mut queues = self.queues.write().await;
        queues
            .get_mut(agent_name)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Peek at pending messages without consuming them.
    #[allow(dead_code)]
    pub async fn peek(&self, agent_name: &str) -> Vec<Message> {
        let queues = self.queues.read().await;
        queues
            .get(agent_name)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get count of pending messages for an agent.
    #[allow(dead_code)]
    pub async fn pending_count(&self, agent_name: &str) -> usize {
        let queues = self.queues.read().await;
        queues.get(agent_name).map(|q| q.len()).unwrap_or(0)
    }

    /// Get full message history.
    pub async fn get_history(&self) -> Vec<Message> {
        let history = self.history.read().await;
        history.clone()
    }

    /// Clear all queues and history.
    #[allow(dead_code)]
    pub async fn clear(&self) {
        let mut queues = self.queues.write().await;
        queues.clear();
        let mut history = self.history.write().await;
        history.clear();
    }
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

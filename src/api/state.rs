//! Shared HTTP server state and live process-owned handles.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use tokio::sync::{RwLock, Semaphore};

use super::{
    admission, auth, conversation_lifecycle, conversations, deployment, idempotency, lifecycle,
};
use crate::engine::eventbus::EventBus;
use crate::engine::input_bridge::InputBridge;
use crate::engine::store::StateStore;

#[derive(Clone, Copy)]
pub struct CachedReadiness {
    pub checked_at: std::time::Instant,
    pub ready: bool,
    pub component: &'static str,
}

pub struct ActiveRun {
    pub eventbus: EventBus,
    pub abort_handle: tokio::task::AbortHandle,
    pub flow: String,
    pub input_bridge: Arc<InputBridge>,
    pub terminal: tokio::sync::watch::Receiver<bool>,
}

pub type ActiveConversationsMap =
    Arc<RwLock<HashMap<(String, String), Arc<conversations::ConversationHandle>>>>;

pub struct AppState {
    pub flows_dir: PathBuf,
    pub runtime_identity: deployment::RuntimeIdentity,
    pub auth: Arc<auth::AuthConfig>,
    pub admission: Arc<admission::AdmissionController>,
    pub lifecycle: lifecycle::LifecycleController,
    pub active_runs: Arc<RwLock<HashMap<String, ActiveRun>>>,
    pub active_conversations: ActiveConversationsMap,
    pub conversation_lifecycles: Arc<conversation_lifecycle::ConversationLifecycleRegistry>,
    pub max_active_conversations: usize,
    pub conversation_permits: Arc<Semaphore>,
    pub max_active_runs: usize,
    pub run_permits: Arc<Semaphore>,
    pub max_active_inspections: usize,
    pub inspection_permits: Arc<Semaphore>,
    pub max_sse_connections: usize,
    pub sse_permits: Arc<Semaphore>,
    pub max_run_lifetime: std::time::Duration,
    pub terminal_persistence_failures: AtomicUsize,
    pub store_maintenance_healthy: AtomicBool,
    pub readiness_cache: tokio::sync::Mutex<Option<CachedReadiness>>,
    pub idempotency: idempotency::IdempotencyConfig,
    pub store: Arc<dyn StateStore>,
}

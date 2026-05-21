//! Audit-log recording helper for state-changing API handlers.
//!
//! Each state-changing handler ends with a single `record(...)` call
//! that captures the outcome. Failures inside the recorder are
//! non-fatal — logged at error level, never bubbled — so the audited
//! action's response is unaffected.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::HeaderMap;

use crate::engine::audit::AuditEvent;
use crate::engine::store::StateStore;

/// Maximum allowed serialized size of the `metadata` field.
const METADATA_MAX_BYTES: usize = 2 * 1024;

/// Maximum length of the `X-Audit-Actor` value after trimming.
const ACTOR_MAX_LEN: usize = 256;

#[allow(clippy::too_many_arguments)]
// TODO(task-8): remove allow when handlers call record
#[allow(dead_code)]
pub async fn record(
    store: &Arc<dyn StateStore>,
    action: &str,
    flow_path: Option<&str>,
    target: Option<&str>,
    headers: &HeaderMap,
    addr: Option<SocketAddr>,
    success: bool,
    status_code: u16,
    metadata: Option<serde_json::Value>,
) {
    let event = AuditEvent {
        id: String::new(), // overwritten by the backend
        timestamp: chrono::Utc::now().to_rfc3339(),
        action: action.to_string(),
        flow_path: flow_path.map(str::to_string),
        target: target.map(str::to_string),
        actor: extract_actor(headers),
        source_ip: extract_source_ip(headers, addr),
        success,
        status_code,
        metadata: metadata.and_then(clamp_metadata),
    };

    if let Err(e) = store.save_audit_event(&event).await {
        tracing::error!(
            action = %event.action,
            target = ?event.target,
            "Audit recorder failed (non-fatal): {e}"
        );
    }
}

fn extract_actor(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("X-Audit-Actor")?.to_str().ok()?.trim();
    if raw.is_empty() || raw.len() > ACTOR_MAX_LEN {
        return None;
    }
    if raw.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(raw.to_string())
}

fn extract_source_ip(headers: &HeaderMap, addr: Option<SocketAddr>) -> Option<String> {
    // Behind a trusted proxy, prefer the first hop of X-Forwarded-For.
    // Gated by IRONCREW_TRUST_PROXY=1 to prevent spoofing in direct-
    // exposure deployments.
    if std::env::var("IRONCREW_TRUST_PROXY")
        .map(|v| v == "1")
        .unwrap_or(false)
        && let Some(xff) = headers.get("X-Forwarded-For").and_then(|v| v.to_str().ok())
    {
        let first = xff.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return Some(first.to_string());
        }
    }
    addr.map(|a| a.ip().to_string())
}

fn clamp_metadata(value: serde_json::Value) -> Option<serde_json::Value> {
    let bytes = serde_json::to_vec(&value).ok()?;
    if bytes.len() > METADATA_MAX_BYTES {
        tracing::warn!(
            size = bytes.len(),
            cap = METADATA_MAX_BYTES,
            "Audit metadata too large; dropping"
        );
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::audit::{AuditEvent, AuditFilter};
    use crate::engine::run_history::{ListRunsFilter, RunRecord, RunStatus, RunSummary};
    use crate::engine::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
    use crate::utils::error::{IronCrewError, Result};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Test store that returns an error on every audit save. Other
    /// methods panic (we only need the audit path for T4).
    struct FailingStore {
        save_calls: Mutex<usize>,
    }

    #[async_trait]
    impl StateStore for FailingStore {
        async fn save_audit_event(&self, _event: &AuditEvent) -> Result<String> {
            *self.save_calls.lock().unwrap() += 1;
            Err(IronCrewError::Validation("forced failure".into()))
        }
        async fn list_audit_events(
            &self,
            _: &AuditFilter,
            _: usize,
            _: usize,
        ) -> Result<Vec<AuditEvent>> {
            unimplemented!()
        }
        async fn count_audit_events(&self, _: &AuditFilter) -> Result<u64> {
            unimplemented!()
        }

        // Boilerplate: other StateStore methods are not exercised in T4.
        async fn save_run_intent(
            &self,
            _: Option<String>,
            _: &str,
            _: &str,
            _: usize,
            _: usize,
            _: &[String],
        ) -> Result<String> {
            unimplemented!()
        }
        async fn update_run_completion(
            &self,
            _: &str,
            _: RunStatus,
            _: &str,
            _: u64,
            _: Vec<crate::engine::task::TaskResult>,
            _: u32,
            _: u32,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn reconcile_abandoned_runs(&self, _: &str) -> Result<usize> {
            unimplemented!()
        }
        async fn get_run(&self, _: &str) -> Result<RunRecord> {
            unimplemented!()
        }
        async fn list_runs_summary(
            &self,
            _: &ListRunsFilter,
            _: usize,
            _: usize,
        ) -> Result<Vec<RunSummary>> {
            unimplemented!()
        }
        async fn count_runs(&self, _: &ListRunsFilter) -> Result<u64> {
            unimplemented!()
        }
        async fn delete_run(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn save_conversation(&self, _: &ConversationRecord) -> Result<()> {
            unimplemented!()
        }
        async fn get_conversation(
            &self,
            _: Option<&str>,
            _: &str,
        ) -> Result<Option<ConversationRecord>> {
            unimplemented!()
        }
        async fn delete_conversation(&self, _: Option<&str>, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn list_conversations(
            &self,
            _: Option<&str>,
            _: usize,
            _: usize,
        ) -> Result<Vec<ConversationSummary>> {
            unimplemented!()
        }
        async fn count_conversations(&self, _: Option<&str>) -> Result<u64> {
            unimplemented!()
        }
        async fn save_dialog_state(&self, _: &DialogStateRecord) -> Result<()> {
            unimplemented!()
        }
        async fn get_dialog_state(
            &self,
            _: Option<&str>,
            _: &str,
        ) -> Result<Option<DialogStateRecord>> {
            unimplemented!()
        }
        async fn delete_dialog_state(&self, _: Option<&str>, _: &str) -> Result<()> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn record_is_nonfatal_on_store_failure() {
        let store: Arc<dyn StateStore> = Arc::new(FailingStore {
            save_calls: Mutex::new(0),
        });
        let headers = HeaderMap::new();

        // Should not panic, should not bubble. The error is logged.
        record(
            &store,
            "flow.run.delete",
            Some("chat-http"),
            Some("run-xyz"),
            &headers,
            None,
            true,
            200,
            None,
        )
        .await;
        // Reaching here means record() returned without panicking.
    }

    #[test]
    fn extract_actor_rejects_too_long_or_control_chars() {
        let mut h = HeaderMap::new();
        h.insert("X-Audit-Actor", "alice@example.com".parse().unwrap());
        assert_eq!(extract_actor(&h).as_deref(), Some("alice@example.com"));

        h.insert("X-Audit-Actor", "  bob  ".parse().unwrap());
        assert_eq!(extract_actor(&h).as_deref(), Some("bob"));

        h.insert("X-Audit-Actor", "".parse().unwrap());
        assert_eq!(extract_actor(&h), None);

        let long = "a".repeat(257);
        h.insert("X-Audit-Actor", long.parse().unwrap());
        assert_eq!(extract_actor(&h), None);
    }

    #[test]
    fn clamp_metadata_drops_oversized_payloads() {
        let small = serde_json::json!({"key": "value"});
        assert!(clamp_metadata(small.clone()).is_some());

        let big_string = "x".repeat(3000);
        let big = serde_json::json!({"k": big_string});
        assert!(clamp_metadata(big).is_none());
    }
}

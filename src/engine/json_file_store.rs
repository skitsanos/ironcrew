//! Owned blocking-worker boundary for the synchronous JSON store core.

use std::time::Duration;

use async_trait::async_trait;

use super::audit::{AuditEvent, AuditFilter};
use super::idempotency::{
    ConversationIdempotencyCommit, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyCompletionOutcome, IdempotencyLimits, IdempotencyLookup,
    IdempotencyUsage, PrincipalId, RunFenceHeartbeat,
};
pub use super::json_file_store_runtime::JsonFileStore;
use super::json_file_store_runtime::JsonFileStoreCore;
use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::StateStore;
use crate::utils::error::Result;

#[async_trait]
impl StateStore for JsonFileStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        self.run_blocking("save_run_intent", move |core| {
            core.save_run_intent_sync(intent)
        })
        .await
    }

    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        let run_id = run_id.to_string();
        self.run_blocking("update_run_completion", move |core| {
            core.update_run_completion_sync(&run_id, completion)
        })
        .await
    }

    async fn update_run_status(&self, run_id: &str, status: RunStatus) -> Result<()> {
        let run_id = run_id.to_string();
        self.run_blocking("update_run_status", move |core| {
            core.update_run_status_sync(&run_id, status)
        })
        .await
    }

    fn instance_id(&self) -> &str {
        StateStore::instance_id(self.inner.as_ref())
    }

    fn run_lease_ttl(&self) -> Duration {
        StateStore::run_lease_ttl(self.inner.as_ref())
    }

    async fn heartbeat_owned_runs(&self) -> Result<usize> {
        self.run_blocking(
            "heartbeat_owned_runs",
            JsonFileStoreCore::heartbeat_owned_runs_sync,
        )
        .await
    }

    async fn health_check(&self) -> Result<()> {
        self.run_blocking("health_check", JsonFileStoreCore::health_check_sync)
            .await
    }

    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        let now = now.to_string();
        self.run_blocking("reconcile_abandoned_runs", move |core| {
            core.reconcile_abandoned_runs_sync(&now)
        })
        .await
    }

    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        let run_id = run_id.to_string();
        self.run_blocking("get_run", move |core| core.get_run_sync(&run_id))
            .await
    }

    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        let filter = filter.clone();
        self.run_blocking("list_runs_summary", move |core| {
            core.list_runs_summary_sync(&filter, limit, offset)
        })
        .await
    }

    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        let filter = filter.clone();
        self.run_blocking("count_runs", move |core| core.count_runs_sync(&filter))
            .await
    }

    async fn delete_run(&self, run_id: &str) -> Result<()> {
        let run_id = run_id.to_string();
        self.run_blocking("delete_run", move |core| core.delete_run_sync(&run_id))
            .await
    }

    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        let principal_id = principal_id.clone();
        let key_hash = key_hash.to_string();
        let request_fingerprint = request_fingerprint.to_string();
        let now = now.to_string();
        self.run_blocking("lookup_idempotency", move |core| {
            core.lookup_idempotency_sync(&principal_id, &key_hash, &request_fingerprint, &now)
        })
        .await
    }

    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        self.run_blocking("claim_idempotency", move |core| {
            core.claim_idempotency_sync(claim, limits)
        })
        .await
    }

    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        let key_hash = key_hash.to_string();
        let attempt_id = attempt_id.to_string();
        let deadline = new_lease_expires_at.to_string();
        self.run_blocking("heartbeat_idempotency", move |core| {
            core.heartbeat_idempotency_sync(&key_hash, &attempt_id, &deadline)
        })
        .await
    }

    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        let run_id = run_id.to_string();
        let key_hash = key_hash.to_string();
        let attempt_id = attempt_id.to_string();
        let deadline = new_lease_expires_at.to_string();
        self.run_blocking("heartbeat_idempotent_run", move |core| {
            core.heartbeat_idempotent_run_sync(&run_id, &key_hash, &attempt_id, &deadline)
        })
        .await
    }

    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        self.run_blocking("complete_idempotency", move |core| {
            core.complete_idempotency_sync(completion, limits)
        })
        .await
    }

    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        let conversation = conversation.clone();
        self.run_blocking("commit_conversation_idempotency", move |core| {
            core.commit_conversation_idempotency_sync(completion, &conversation, limits)
        })
        .await
    }

    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        let values = (
            key_hash.to_string(),
            attempt_id.to_string(),
            completed_at.to_string(),
            expires_at.to_string(),
        );
        self.run_blocking("mark_idempotency_indeterminate", move |core| {
            core.mark_idempotency_indeterminate_sync(&values)
        })
        .await
    }

    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool> {
        let key_hash = key_hash.to_string();
        let attempt_id = attempt_id.to_string();
        self.run_blocking("release_idempotency", move |core| {
            core.release_idempotency_sync(&key_hash, &attempt_id)
        })
        .await
    }

    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize> {
        let now = now.to_string();
        self.run_blocking("prune_idempotency", move |core| {
            core.prune_idempotency_sync(&now, limit)
        })
        .await
    }

    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        let principal_id = principal_id.clone();
        self.run_blocking("idempotency_usage", move |core| {
            core.idempotency_usage_sync(&principal_id, limits)
        })
        .await
    }

    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        let record = record.clone();
        self.run_blocking("save_conversation", move |core| {
            core.save_conversation_sync(&record)
        })
        .await
    }

    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        let flow_path = flow_path.map(str::to_string);
        let id = id.to_string();
        self.run_blocking("get_conversation", move |core| {
            core.get_conversation_sync(flow_path.as_deref(), &id)
        })
        .await
    }

    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let flow_path = flow_path.map(str::to_string);
        let id = id.to_string();
        self.run_blocking("delete_conversation", move |core| {
            core.delete_conversation_sync(flow_path.as_deref(), &id)
        })
        .await
    }

    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        let flow_path = flow_path.map(str::to_string);
        self.run_blocking("list_conversations", move |core| {
            core.list_conversations_sync(flow_path.as_deref(), limit, offset)
        })
        .await
    }

    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64> {
        let flow_path = flow_path.map(str::to_string);
        self.run_blocking("count_conversations", move |core| {
            core.count_conversations_sync(flow_path.as_deref())
        })
        .await
    }

    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64> {
        let record = record.clone();
        self.run_blocking("save_dialog_state", move |core| {
            core.save_dialog_state_sync(&record)
        })
        .await
    }

    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        let flow_path = flow_path.map(str::to_string);
        let id = id.to_string();
        self.run_blocking("get_dialog_state", move |core| {
            core.get_dialog_state_sync(flow_path.as_deref(), &id)
        })
        .await
    }

    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        let flow_path = flow_path.map(str::to_string);
        let id = id.to_string();
        self.run_blocking("delete_dialog_state", move |core| {
            core.delete_dialog_state_sync(flow_path.as_deref(), &id)
        })
        .await
    }

    async fn save_audit_event(&self, event: &AuditEvent) -> Result<String> {
        let event = event.clone();
        self.run_blocking("save_audit_event", move |core| {
            core.save_audit_event_sync(&event)
        })
        .await
    }

    async fn list_audit_events(
        &self,
        filter: &AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditEvent>> {
        let filter = filter.clone();
        self.run_blocking("list_audit_events", move |core| {
            core.list_audit_events_sync(&filter, limit, offset)
        })
        .await
    }

    async fn count_audit_events(&self, filter: &AuditFilter) -> Result<u64> {
        let filter = filter.clone();
        self.run_blocking("count_audit_events", move |core| {
            core.count_audit_events_sync(&filter)
        })
        .await
    }
}

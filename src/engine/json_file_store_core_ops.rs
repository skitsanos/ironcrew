//! Synchronous calls into the JSON persistence core.

use futures::executor::block_on;

use super::audit::{AuditEvent, AuditFilter};
use super::idempotency::{
    ConversationIdempotencyCommit, IdempotencyClaim, IdempotencyClaimOutcome,
    IdempotencyCompletion, IdempotencyCompletionOutcome, IdempotencyLimits, IdempotencyLookup,
    IdempotencyUsage, PrincipalId, RunFenceHeartbeat,
};
use super::json_file_store_runtime::JsonFileStoreCore;
use super::run_history::{
    ListRunsFilter, RunCompletion, RunIntent, RunRecord, RunStatus, RunSummary, RunTransition,
};
use super::sessions::{ConversationRecord, ConversationSummary, DialogStateRecord};
use super::store::StateStore;
use crate::utils::error::Result;

impl JsonFileStoreCore {
    pub(super) fn save_run_intent_sync(&self, intent: RunIntent) -> Result<String> {
        block_on(StateStore::save_run_intent(self, intent))
    }

    pub(super) fn update_run_completion_sync(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        block_on(StateStore::update_run_completion(self, run_id, completion))
    }

    pub(super) fn update_run_status_sync(&self, run_id: &str, status: RunStatus) -> Result<()> {
        block_on(StateStore::update_run_status(self, run_id, status))
    }

    pub(super) fn heartbeat_owned_runs_sync(&self) -> Result<usize> {
        block_on(StateStore::heartbeat_owned_runs(self))
    }

    pub(super) fn health_check_sync(&self) -> Result<()> {
        block_on(StateStore::health_check(self))
    }

    pub(super) fn reconcile_abandoned_runs_sync(&self, now: &str) -> Result<usize> {
        block_on(StateStore::reconcile_abandoned_runs(self, now))
    }

    pub(super) fn get_run_sync(&self, run_id: &str) -> Result<RunRecord> {
        block_on(StateStore::get_run(self, run_id))
    }

    pub(super) fn list_runs_summary_sync(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        block_on(StateStore::list_runs_summary(self, filter, limit, offset))
    }

    pub(super) fn count_runs_sync(&self, filter: &ListRunsFilter) -> Result<u64> {
        block_on(StateStore::count_runs(self, filter))
    }

    pub(super) fn delete_run_sync(&self, run_id: &str) -> Result<()> {
        block_on(StateStore::delete_run(self, run_id))
    }

    pub(super) fn lookup_idempotency_sync(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        block_on(StateStore::lookup_idempotency_for_principal(
            self,
            principal_id,
            key_hash,
            request_fingerprint,
            now,
        ))
    }

    pub(super) fn claim_idempotency_sync(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        block_on(StateStore::claim_idempotency_with_limits(
            self, claim, limits,
        ))
    }

    pub(super) fn heartbeat_idempotency_sync(
        &self,
        key_hash: &str,
        attempt_id: &str,
        deadline: &str,
    ) -> Result<bool> {
        block_on(StateStore::heartbeat_idempotency(
            self, key_hash, attempt_id, deadline,
        ))
    }

    pub(super) fn heartbeat_idempotent_run_sync(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        deadline: &str,
    ) -> Result<RunFenceHeartbeat> {
        block_on(StateStore::heartbeat_idempotent_run(
            self, run_id, key_hash, attempt_id, deadline,
        ))
    }

    pub(super) fn complete_idempotency_sync(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        block_on(StateStore::complete_idempotency_with_limits(
            self, completion, limits,
        ))
    }

    pub(super) fn commit_conversation_idempotency_sync(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        block_on(StateStore::commit_conversation_idempotency_with_limits(
            self,
            completion,
            conversation,
            limits,
        ))
    }

    pub(super) fn mark_idempotency_indeterminate_sync(
        &self,
        values: &(String, String, String, String),
    ) -> Result<bool> {
        block_on(StateStore::mark_idempotency_indeterminate(
            self, &values.0, &values.1, &values.2, &values.3,
        ))
    }

    pub(super) fn release_idempotency_sync(
        &self,
        key_hash: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        block_on(StateStore::release_idempotency(self, key_hash, attempt_id))
    }

    pub(super) fn prune_idempotency_sync(&self, now: &str, limit: usize) -> Result<usize> {
        block_on(StateStore::prune_idempotency(self, now, limit))
    }

    pub(super) fn idempotency_usage_sync(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        block_on(StateStore::idempotency_usage(self, principal_id, limits))
    }

    pub(super) fn save_conversation_sync(&self, record: &ConversationRecord) -> Result<u64> {
        block_on(StateStore::save_conversation(self, record))
    }

    pub(super) fn get_conversation_sync(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        block_on(StateStore::get_conversation(self, flow_path, id))
    }

    pub(super) fn delete_conversation_sync(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        block_on(StateStore::delete_conversation(self, flow_path, id))
    }

    pub(super) fn list_conversations_sync(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        block_on(StateStore::list_conversations(
            self, flow_path, limit, offset,
        ))
    }

    pub(super) fn count_conversations_sync(&self, flow_path: Option<&str>) -> Result<u64> {
        block_on(StateStore::count_conversations(self, flow_path))
    }

    pub(super) fn save_dialog_state_sync(&self, record: &DialogStateRecord) -> Result<u64> {
        block_on(StateStore::save_dialog_state(self, record))
    }

    pub(super) fn get_dialog_state_sync(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        block_on(StateStore::get_dialog_state(self, flow_path, id))
    }

    pub(super) fn delete_dialog_state_sync(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        block_on(StateStore::delete_dialog_state(self, flow_path, id))
    }

    pub(super) fn save_audit_event_sync(&self, event: &AuditEvent) -> Result<String> {
        block_on(StateStore::save_audit_event(self, event))
    }

    pub(super) fn list_audit_events_sync(
        &self,
        filter: &AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditEvent>> {
        block_on(StateStore::list_audit_events(self, filter, limit, offset))
    }

    pub(super) fn count_audit_events_sync(&self, filter: &AuditFilter) -> Result<u64> {
        block_on(StateStore::count_audit_events(self, filter))
    }
}

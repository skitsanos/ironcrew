use super::*;
#[async_trait]
impl StateStore for PostgresStore {
    async fn save_run_intent(&self, intent: RunIntent) -> Result<String> {
        self.insert_run_intent(intent).await
    }
    async fn update_run_completion(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        self.finish_run(run_id, completion).await
    }
    async fn update_run_status(
        &self,
        run_id: &str,
        status: crate::engine::run_history::RunStatus,
    ) -> Result<()> {
        self.set_run_status(run_id, status).await
    }
    fn instance_id(&self) -> &str {
        self.lease.instance_id()
    }
    fn postgres_pool_usage(&self) -> Option<crate::engine::store::PostgresPoolUsage> {
        let open_connections = self.pool.size();
        let idle_connections = u32::try_from(self.pool.num_idle())
            .unwrap_or(u32::MAX)
            .min(open_connections);
        Some(crate::engine::store::PostgresPoolUsage {
            open_connections,
            in_use_connections: open_connections.saturating_sub(idle_connections),
            connection_limit: self.pool.options().get_max_connections(),
        })
    }
    fn run_lease_ttl(&self) -> Duration {
        self.lease.ttl()
    }
    fn run_maintenance_watchdog(&self) -> Option<Duration> {
        Some(crate::engine::store::run_maintenance_timeout(
            self.lease.ttl(),
        ))
    }
    fn supports_durable_human_input(&self) -> bool {
        self.human_input_keyring.is_some()
    }
    fn event_journal_scope(&self) -> EventJournalScope {
        EventJournalScope::SharedStore
    }
    fn conversation_coordination_scope(&self) -> ConversationCoordinationScope {
        ConversationCoordinationScope::SharedStore
    }
    fn event_journal_config(&self) -> RunEventJournalConfig {
        self.run_event_journal_config.clone()
    }
    async fn append_run_events(
        &self,
        batch: &RunEventAppendBatch,
    ) -> Result<RunEventAppendOutcome> {
        self.append_run_event_journal(batch).await
    }
    async fn read_run_events(
        &self,
        flow: &str,
        run_id: &str,
        after_sequence: u64,
    ) -> Result<RunEventPage> {
        self.read_run_events_journal(flow, run_id, after_sequence)
            .await
    }
    async fn heartbeat_owned_runs(&self) -> Result<usize> {
        self.heartbeat_owned_runs_maintenance().await
    }
    async fn health_check(&self) -> Result<()> {
        self.health_check_maintenance().await
    }
    async fn reconcile_abandoned_runs(&self, now: &str) -> Result<usize> {
        self.reconcile_abandoned_runs_maintenance(now).await
    }
    async fn get_run(&self, run_id: &str) -> Result<RunRecord> {
        self.load_run(run_id).await
    }
    async fn list_runs_summary(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        self.load_run_summaries(filter, limit, offset).await
    }
    async fn count_runs(&self, filter: &ListRunsFilter) -> Result<u64> {
        self.load_run_count(filter).await
    }
    async fn delete_run(&self, run_id: &str) -> Result<()> {
        self.remove_run(run_id).await
    }
    async fn lookup_idempotency_for_principal(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        self.lookup_idempotency_for_principal_record(
            principal_id,
            key_hash,
            request_fingerprint,
            now,
        )
        .await
    }
    async fn claim_idempotency_with_limits(
        &self,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyClaimOutcome> {
        self.claim_idempotency_record(claim, limits).await
    }
    async fn heartbeat_idempotency(
        &self,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<bool> {
        self.heartbeat_idempotency_record(key_hash, attempt_id, new_lease_expires_at)
            .await
    }
    async fn heartbeat_idempotent_run(
        &self,
        run_id: &str,
        key_hash: &str,
        attempt_id: &str,
        new_lease_expires_at: &str,
    ) -> Result<RunFenceHeartbeat> {
        self.heartbeat_idempotent_run_record(run_id, key_hash, attempt_id, new_lease_expires_at)
            .await
    }
    async fn begin_owner_drain(&self) -> Result<usize> {
        self.begin_owner_drain_maintenance().await
    }
    async fn request_run_cancellation(
        &self,
        run_id: &str,
        flow: &str,
    ) -> Result<RunCancellationRequest> {
        self.request_run_cancellation_maintenance(run_id, flow)
            .await
    }
    async fn register_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputRegistrationOutcome> {
        self.register_human_input_record(registration).await
    }
    async fn list_human_inputs(&self, flow: &str, run_id: &str) -> Result<HumanInputListOutcome> {
        self.list_human_input_records(flow, run_id).await
    }
    async fn answer_human_input(
        &self,
        flow: &str,
        run_id: &str,
        question_id: &str,
        answer: &serde_json::Value,
    ) -> Result<HumanInputAnswerOutcome> {
        self.answer_human_input_record(flow, run_id, question_id, answer)
            .await
    }
    async fn read_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<HumanInputReadOutcome> {
        self.read_human_input_record(registration).await
    }
    async fn close_human_input(
        &self,
        registration: &DurableHumanInputRegistration,
    ) -> Result<bool> {
        self.close_human_input_record(registration).await
    }
    async fn complete_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyCompletionOutcome> {
        self.complete_idempotency_record(completion, limits).await
    }
    async fn commit_conversation_idempotency_with_limits(
        &self,
        completion: IdempotencyCompletion,
        conversation: &ConversationRecord,
        limits: IdempotencyLimits,
    ) -> Result<ConversationIdempotencyCommit> {
        self.commit_conversation_idempotency_record(completion, conversation, limits)
            .await
    }
    async fn mark_idempotency_indeterminate(
        &self,
        key_hash: &str,
        attempt_id: &str,
        completed_at: &str,
        expires_at: &str,
    ) -> Result<bool> {
        self.mark_idempotency_indeterminate_record(key_hash, attempt_id, completed_at, expires_at)
            .await
    }
    async fn release_idempotency(&self, key_hash: &str, attempt_id: &str) -> Result<bool> {
        self.release_idempotency_record(key_hash, attempt_id).await
    }
    async fn prune_idempotency(&self, now: &str, limit: usize) -> Result<usize> {
        self.prune_idempotency_records(now, limit).await
    }
    async fn idempotency_usage(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        self.idempotency_usage_record(principal_id, limits).await
    }
    async fn save_conversation(&self, record: &ConversationRecord) -> Result<u64> {
        self.save_conversation_record(record).await
    }
    async fn get_conversation(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<ConversationRecord>> {
        self.get_conversation_record(flow_path, id).await
    }
    async fn delete_conversation(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        self.delete_conversation_record(flow_path, id).await
    }
    async fn list_conversations(
        &self,
        flow_path: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<ConversationSummary>> {
        self.list_conversation_records(flow_path, limit, offset)
            .await
    }
    async fn count_conversations(&self, flow_path: Option<&str>) -> Result<u64> {
        self.count_conversation_records(flow_path).await
    }
    async fn save_dialog_state(&self, record: &DialogStateRecord) -> Result<u64> {
        self.save_dialog_state_record(record).await
    }
    async fn get_dialog_state(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        self.get_dialog_state_record(flow_path, id).await
    }
    async fn delete_dialog_state(&self, flow_path: Option<&str>, id: &str) -> Result<()> {
        self.delete_dialog_state_record(flow_path, id).await
    }
    async fn save_audit_event(&self, event: &crate::engine::audit::AuditEvent) -> Result<String> {
        self.save_audit_event_record(event).await
    }
    async fn list_audit_events(
        &self,
        filter: &crate::engine::audit::AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<crate::engine::audit::AuditEvent>> {
        self.list_audit_event_records(filter, limit, offset).await
    }
    async fn count_audit_events(&self, filter: &crate::engine::audit::AuditFilter) -> Result<u64> {
        self.count_audit_event_records(filter).await
    }
}

mod persistence;
mod validation;

use crate::engine::run_events::{RunEventAppendBatch, RunEventAppendOutcome};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn append_run_event_journal(
        &self,
        batch: &RunEventAppendBatch,
    ) -> Result<RunEventAppendOutcome> {
        batch.validate(&self.run_event_journal_config)?;
        if batch.owner_instance_id != self.lease.instance_id() {
            return Err(IronCrewError::Conflict(format!(
                "Run-event batch owner '{}' does not match this store instance",
                batch.owner_instance_id
            )));
        }
        let sequence_values: Vec<i64> = batch
            .entries
            .iter()
            .map(|entry| {
                i64::try_from(entry.sequence).map_err(|_| {
                    IronCrewError::Validation("Run-event sequence exceeds PostgreSQL BIGINT".into())
                })
            })
            .collect::<Result<_>>()?;

        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run-event append transaction failed: {error}"
            ))
        })?;
        self.configure_run_event_transaction(&mut tx).await?;
        self.lock_run_event_usage(&mut tx).await?;
        let target = self.lock_run_event_append_target(&mut tx, batch).await?;
        let mut pruned = self.prune_expired_run_events(&mut tx).await?;
        let state = self.initialize_run_event_state(&mut tx, batch).await?;
        let existing = self
            .load_existing_run_events(&mut tx, batch, &sequence_values)
            .await?;
        let (duplicate_events, new_entries) =
            Self::partition_new_run_event_entries(batch, &state, &existing)?;
        Self::validate_new_run_event_entries(batch, &target, &state, &new_entries)?;
        let accounted = self.account_run_event_append(&mut tx, new_entries).await?;
        if accounted.event_count > 0 {
            self.persist_run_event_append(&mut tx, batch, state, &accounted, &mut pruned)
                .await?;
        }

        let state = self
            .run_event_state_for_update(&mut tx, &batch.run_id)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL run-event state is missing".into())
            })?;
        let bounds = self
            .run_event_bounds(&mut tx, &batch.run_id, &state)
            .await?;
        let run_eviction = pruned.for_run(&batch.run_id);
        let outcome = RunEventAppendOutcome {
            appended_events: accounted.event_count,
            duplicate_events,
            evicted_events: run_eviction.events,
            evicted_bytes: run_eviction.bytes,
            eviction_gap: run_eviction.gap(),
            bounds,
        };
        outcome.validate()?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run-event append commit failed: {error}"
            ))
        })?;
        Ok(outcome)
    }
}

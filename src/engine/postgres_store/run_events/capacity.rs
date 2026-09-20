use sqlx::Row;

use crate::engine::run_events::RunEventGapReason;
use crate::utils::error::{IronCrewError, Result};

use super::super::codecs::nonnegative_u64;
use super::super::{MAX_EVICTED_RUN_EVENTS_PER_APPEND, PostgresStore};
use super::types::{RunEventDeleteCandidate, RunEventPruneSummary};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn evict_run_event_capacity(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        events_to_free: u64,
        bytes_to_free: u64,
    ) -> Result<RunEventPruneSummary> {
        if events_to_free == 0 && bytes_to_free == 0 {
            return Ok(RunEventPruneSummary::default());
        }
        let sql = format!(
            "SELECT run_id, sequence, accounted_bytes AS payload_bytes FROM {} \
             WHERE run_id = $1 ORDER BY sequence LIMIT $2 FOR UPDATE",
            self.run_events_table
        );
        let batch_limit =
            i64::try_from(self.run_event_journal_config.prune_batch).map_err(|_| {
                IronCrewError::Validation("Run-event prune batch exceeds BIGINT".into())
            })?;
        let mut remaining_events = events_to_free;
        let mut remaining_bytes = bytes_to_free;
        let mut total_evicted = 0u64;
        let mut summary = RunEventPruneSummary::default();
        while remaining_events > 0 || remaining_bytes > 0 {
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(run_id)
                .bind(batch_limit)
                .fetch_all(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL per-run event eviction selection failed: {error}"
                    ))
                })?;
            let mut candidates = Vec::new();
            let mut freed_bytes = 0u64;
            for row in rows {
                let candidate = RunEventDeleteCandidate {
                    run_id: row.try_get("run_id").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event run_id column: {error}"))
                    })?,
                    sequence: nonnegative_u64(
                        "evicted sequence",
                        row.try_get("sequence").map_err(|error| {
                            IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                        })?,
                    )?,
                    payload_bytes: nonnegative_u64(
                        "evicted payload byte count",
                        row.try_get("payload_bytes").map_err(|error| {
                            IronCrewError::Validation(format!(
                                "Run-event payload_bytes column: {error}"
                            ))
                        })?,
                    )?,
                };
                freed_bytes = freed_bytes.saturating_add(candidate.payload_bytes);
                candidates.push(candidate);
                if candidates.len() as u64 >= remaining_events && freed_bytes >= remaining_bytes {
                    break;
                }
            }
            if candidates.is_empty()
                || total_evicted.saturating_add(candidates.len() as u64)
                    > MAX_EVICTED_RUN_EVENTS_PER_APPEND
            {
                return Err(IronCrewError::Conflict(format!(
                    "Run-event per-run capacity for '{run_id}' cannot be reclaimed within the bounded {MAX_EVICTED_RUN_EVENTS_PER_APPEND}-row append budget"
                )));
            }
            let freed_events = candidates.len() as u64;
            summary.merge(
                self.delete_run_event_candidates(
                    tx,
                    &candidates,
                    RunEventGapReason::WriterBackpressure,
                )
                .await?,
            );
            total_evicted = total_evicted.saturating_add(freed_events);
            remaining_events = remaining_events.saturating_sub(freed_events);
            remaining_bytes = remaining_bytes.saturating_sub(freed_bytes);
        }
        Ok(summary)
    }

    pub(in crate::engine::postgres_store) async fn evict_global_run_event_capacity(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        events_to_free: u64,
        bytes_to_free: u64,
    ) -> Result<RunEventPruneSummary> {
        if events_to_free == 0 && bytes_to_free == 0 {
            return Ok(RunEventPruneSummary::default());
        }
        let sql = format!(
            "SELECT run_id, sequence, accounted_bytes AS payload_bytes FROM {} \
             ORDER BY created_at, run_id, sequence LIMIT $1 FOR UPDATE",
            self.run_events_table
        );
        let batch_limit =
            i64::try_from(self.run_event_journal_config.prune_batch).map_err(|_| {
                IronCrewError::Validation("Run-event prune batch exceeds BIGINT".into())
            })?;
        let mut remaining_events = events_to_free;
        let mut remaining_bytes = bytes_to_free;
        let mut total_evicted = 0u64;
        let mut summary = RunEventPruneSummary::default();
        while remaining_events > 0 || remaining_bytes > 0 {
            let rows = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(batch_limit)
                .fetch_all(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL global event eviction selection failed: {error}"
                    ))
                })?;
            let mut candidates = Vec::new();
            let mut freed_bytes = 0u64;
            for row in rows {
                let candidate = RunEventDeleteCandidate {
                    run_id: row.try_get("run_id").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event run_id column: {error}"))
                    })?,
                    sequence: nonnegative_u64(
                        "globally evicted sequence",
                        row.try_get("sequence").map_err(|error| {
                            IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                        })?,
                    )?,
                    payload_bytes: nonnegative_u64(
                        "globally evicted payload byte count",
                        row.try_get("payload_bytes").map_err(|error| {
                            IronCrewError::Validation(format!(
                                "Run-event payload_bytes column: {error}"
                            ))
                        })?,
                    )?,
                };
                freed_bytes = freed_bytes.saturating_add(candidate.payload_bytes);
                candidates.push(candidate);
                if candidates.len() as u64 >= remaining_events && freed_bytes >= remaining_bytes {
                    break;
                }
            }
            if candidates.is_empty()
                || total_evicted.saturating_add(candidates.len() as u64)
                    > MAX_EVICTED_RUN_EVENTS_PER_APPEND
            {
                return Err(IronCrewError::Conflict(format!(
                    "Run-event global capacity cannot be reclaimed within the bounded {MAX_EVICTED_RUN_EVENTS_PER_APPEND}-row append budget"
                )));
            }
            let freed_events = candidates.len() as u64;
            summary.merge(
                self.delete_run_event_candidates(
                    tx,
                    &candidates,
                    RunEventGapReason::GlobalCapacity,
                )
                .await?,
            );
            total_evicted = total_evicted.saturating_add(freed_events);
            remaining_events = remaining_events.saturating_sub(freed_events);
            remaining_bytes = remaining_bytes.saturating_sub(freed_bytes);
        }
        Ok(summary)
    }
}

use std::collections::BTreeMap;

use sqlx::Row;

use crate::engine::run_events::RunEventGapReason;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::{nonnegative_u64, run_event_gap_reason_db};
use super::types::{RunEventDeleteCandidate, RunEventPruneSummary, RunEventRunEviction};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn delete_run_event_candidates(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        candidates: &[RunEventDeleteCandidate],
        reason: RunEventGapReason,
    ) -> Result<RunEventPruneSummary> {
        if candidates.is_empty() {
            return Ok(RunEventPruneSummary::default());
        }
        let run_ids: Vec<String> = candidates
            .iter()
            .map(|candidate| candidate.run_id.clone())
            .collect();
        let sequences: Vec<i64> = candidates
            .iter()
            .map(|candidate| {
                i64::try_from(candidate.sequence).map_err(|_| {
                    IronCrewError::Validation("Run-event sequence exceeds PostgreSQL BIGINT".into())
                })
            })
            .collect::<Result<_>>()?;
        let sql = format!(
            "DELETE FROM {events} AS event USING (\
                 SELECT * FROM unnest($1::text[], $2::bigint[]) \
                 AS selected(run_id, sequence)\
             ) AS selected \
             WHERE event.run_id = selected.run_id \
               AND event.sequence = selected.sequence \
             RETURNING event.run_id, event.sequence, event.accounted_bytes AS payload_bytes",
            events = self.run_events_table
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&run_ids)
            .bind(&sequences)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event bounded prune failed: {error}"
                ))
            })?;
        if rows.len() != candidates.len() {
            return Err(IronCrewError::Conflict(
                "Run-event rows changed during bounded pruning".into(),
            ));
        }

        let mut deleted_by_run: BTreeMap<String, RunEventRunEviction> = BTreeMap::new();
        for row in rows {
            let run_id: String = row.try_get("run_id").map_err(|error| {
                IronCrewError::Validation(format!("Run-event run_id column: {error}"))
            })?;
            let sequence = nonnegative_u64(
                "deleted sequence",
                row.try_get::<i64, _>("sequence").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                })?,
            )?;
            let payload_bytes = nonnegative_u64(
                "deleted payload byte count",
                row.try_get::<i64, _>("payload_bytes").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event payload_bytes column: {error}"))
                })?,
            )?;
            let eviction = deleted_by_run.entry(run_id).or_default();
            eviction.events = eviction.events.saturating_add(1);
            eviction.bytes = eviction.bytes.saturating_add(payload_bytes);
            if eviction.first_sequence == 0 {
                eviction.first_sequence = sequence;
            } else {
                eviction.first_sequence = eviction.first_sequence.min(sequence);
            }
            eviction.last_sequence = eviction.last_sequence.max(sequence);
        }

        for (run_id, eviction) in &mut deleted_by_run {
            let state = self
                .run_event_state_for_update(tx, run_id)
                .await?
                .ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event state for '{run_id}' is missing"
                    ))
                })?;
            eviction.previous_dropped_through = state.dropped_through;
            let retained_events = state
                .retained_events
                .checked_sub(eviction.events)
                .ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event count accounting underflow for '{run_id}'"
                    ))
                })?;
            let retained_bytes = state
                .retained_bytes
                .checked_sub(eviction.bytes)
                .ok_or_else(|| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event byte accounting underflow for '{run_id}'"
                    ))
                })?;
            let earliest_sql = format!(
                "SELECT MIN(sequence) FROM {} WHERE run_id = $1",
                self.run_events_table
            );
            let earliest: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(earliest_sql))
                .bind(run_id)
                .fetch_one(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event retained-bound lookup failed: {error}"
                    ))
                })?;
            let new_dropped_through = if retained_events == 0 {
                state.latest_sequence
            } else {
                let earliest = earliest
                    .ok_or_else(|| {
                        IronCrewError::Validation(
                            "PostgreSQL run-event accounting retained rows are missing".into(),
                        )
                    })
                    .and_then(|value| nonnegative_u64("earliest retained sequence", value))?;
                state.dropped_through.max(earliest.saturating_sub(1))
            };
            eviction.new_dropped_through = new_dropped_through;
            if new_dropped_through > state.dropped_through {
                eviction.reason = Some(reason);
            }
            let eviction_reason = if new_dropped_through > state.dropped_through {
                Some(run_event_gap_reason_db(reason))
            } else {
                state.eviction_reason.map(run_event_gap_reason_db)
            };
            let journal_complete =
                state.journal_complete && eviction.last_sequence <= new_dropped_through;
            let update_sql = format!(
                "UPDATE {} SET retained_events = $1, retained_bytes = $2, \
                     dropped_through = $3, eviction_reason = $4, \
                     journal_complete = $5, updated_at = clock_timestamp() \
                 WHERE run_id = $6",
                self.run_event_state_table
            );
            let updated = sqlx::query(sqlx::AssertSqlSafe(update_sql))
                .bind(i64::try_from(retained_events).map_err(|_| {
                    IronCrewError::Validation("Run-event retained count exceeds BIGINT".into())
                })?)
                .bind(i64::try_from(retained_bytes).map_err(|_| {
                    IronCrewError::Validation("Run-event retained bytes exceed BIGINT".into())
                })?)
                .bind(i64::try_from(new_dropped_through).map_err(|_| {
                    IronCrewError::Validation("Run-event dropped boundary exceeds BIGINT".into())
                })?)
                .bind(eviction_reason)
                .bind(journal_complete)
                .bind(run_id)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event state prune update failed: {error}"
                    ))
                })?;
            if updated.rows_affected() != 1 {
                return Err(IronCrewError::Conflict(format!(
                    "Run-event state for '{run_id}' changed during pruning"
                )));
            }
        }

        Ok(RunEventPruneSummary {
            by_run: deleted_by_run,
        })
    }

    pub(in crate::engine::postgres_store) async fn prune_expired_run_events(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<RunEventPruneSummary> {
        let sql = format!(
            "SELECT event.run_id, event.sequence, \
                    event.accounted_bytes AS payload_bytes \
             FROM {events} AS event \
             WHERE event.expires_at <= clock_timestamp() \
             ORDER BY event.expires_at, event.run_id, event.sequence \
             LIMIT $1 FOR UPDATE OF event",
            events = self.run_events_table
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(
                i64::try_from(self.run_event_journal_config.prune_batch).map_err(|_| {
                    IronCrewError::Validation("Run-event prune batch exceeds BIGINT".into())
                })?,
            )
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL expired run-event selection failed: {error}"
                ))
            })?;
        let candidates = rows
            .into_iter()
            .map(|row| {
                Ok(RunEventDeleteCandidate {
                    run_id: row.try_get("run_id").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event run_id column: {error}"))
                    })?,
                    sequence: nonnegative_u64(
                        "expired sequence",
                        row.try_get("sequence").map_err(|error| {
                            IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                        })?,
                    )?,
                    payload_bytes: nonnegative_u64(
                        "expired payload byte count",
                        row.try_get("payload_bytes").map_err(|error| {
                            IronCrewError::Validation(format!(
                                "Run-event payload_bytes column: {error}"
                            ))
                        })?,
                    )?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.delete_run_event_candidates(tx, &candidates, RunEventGapReason::Retention)
            .await
    }
}

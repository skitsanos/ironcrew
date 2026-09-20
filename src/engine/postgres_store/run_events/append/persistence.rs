use crate::engine::run_events::{RunEventAppendBatch, RunEventAppendEntry};
use crate::utils::error::{IronCrewError, Result};

use super::super::super::codecs::nonnegative_u64;
use super::super::super::{MIN_ACCOUNTED_RUN_EVENT_BYTES, PostgresStore};
use super::super::{AccountedRunEventBatch, RunEventPruneSummary, RunEventStateRow};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn account_run_event_append(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        entries: Vec<RunEventAppendEntry>,
    ) -> Result<AccountedRunEventBatch> {
        let event_count = u64::try_from(entries.len())
            .map_err(|_| IronCrewError::Validation("Run-event append count exceeds u64".into()))?;
        let serialized_payloads = entries
            .iter()
            .map(|entry| {
                serde_json::to_string(&entry.payload).map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Run-event payload serialization failed: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let declared_bytes = entries
            .iter()
            .map(|entry| {
                i64::try_from(entry.payload_bytes).map_err(|_| {
                    IronCrewError::Validation(
                        "Run-event payload byte count exceeds PostgreSQL BIGINT".into(),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let accounted_bytes: Vec<i64> = if entries.is_empty() {
            Vec::new()
        } else {
            let sql = format!(
                "SELECT GREATEST(item.declared_bytes, \
                         octet_length(item.payload::jsonb::text)::BIGINT, \
                         {MIN_ACCOUNTED_RUN_EVENT_BYTES}::BIGINT) \
                 FROM unnest($1::text[], $2::bigint[]) WITH ORDINALITY \
                      AS item(payload, declared_bytes, ordinal) \
                 ORDER BY item.ordinal"
            );
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(&serialized_payloads)
                .bind(&declared_bytes)
                .fetch_all(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event payload accounting failed: {error}"
                    ))
                })?
        };
        if accounted_bytes.len() != entries.len() {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event payload accounting returned an incomplete batch".into(),
            ));
        }
        let byte_count = accounted_bytes.iter().try_fold(0u64, |total, bytes| {
            let bytes = nonnegative_u64("accounted payload byte count", *bytes)?;
            total.checked_add(bytes).ok_or_else(|| {
                IronCrewError::Validation("Run-event append byte count overflow".into())
            })
        })?;
        if byte_count > self.run_event_journal_config.max_bytes_per_run as u64
            || byte_count > self.run_event_journal_config.max_total_bytes
        {
            return Err(IronCrewError::Validation(
                "PostgreSQL-accounted run-event append exceeds a configured byte limit".into(),
            ));
        }
        Ok(AccountedRunEventBatch {
            entries,
            serialized_payloads,
            event_count,
            byte_count,
        })
    }

    pub(in crate::engine::postgres_store) async fn persist_run_event_append(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch: &RunEventAppendBatch,
        state: RunEventStateRow,
        accounted: &AccountedRunEventBatch,
        pruned: &mut RunEventPruneSummary,
    ) -> Result<()> {
        let future_events = state
            .retained_events
            .checked_add(accounted.event_count)
            .ok_or_else(|| IronCrewError::Validation("Run-event per-run count overflow".into()))?;
        let future_bytes = state
            .retained_bytes
            .checked_add(accounted.byte_count)
            .ok_or_else(|| IronCrewError::Validation("Run-event per-run bytes overflow".into()))?;
        pruned.merge(
            self.evict_run_event_capacity(
                tx,
                &batch.run_id,
                future_events
                    .saturating_sub(self.run_event_journal_config.max_events_per_run as u64),
                future_bytes.saturating_sub(self.run_event_journal_config.max_bytes_per_run as u64),
            )
            .await?,
        );

        let usage = self.lock_run_event_usage(tx).await?;
        let future_global_events = usage
            .retained_events
            .checked_add(accounted.event_count)
            .ok_or_else(|| IronCrewError::Validation("Run-event global count overflow".into()))?;
        let future_global_bytes = usage
            .retained_bytes
            .checked_add(accounted.byte_count)
            .ok_or_else(|| IronCrewError::Validation("Run-event global bytes overflow".into()))?;
        pruned.merge(
            self.evict_global_run_event_capacity(
                tx,
                future_global_events.saturating_sub(self.run_event_journal_config.max_total_events),
                future_global_bytes.saturating_sub(self.run_event_journal_config.max_total_bytes),
            )
            .await?,
        );
        let state = self
            .run_event_state_for_update(tx, &batch.run_id)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL run-event state is missing".into())
            })?;

        let (created_at, expires_at) = self
            .database_clock_with_deadline(
                tx,
                self.run_event_journal_config.retention.as_secs(),
                "run-event retention",
            )
            .await?;
        let insert_sql = format!(
            "INSERT INTO {} (run_id, sequence, event_type, payload, payload_bytes, \
                 created_at, expires_at) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6::timestamptz, $7::timestamptz)",
            self.run_events_table
        );
        for (entry, payload) in accounted.entries.iter().zip(&accounted.serialized_payloads) {
            sqlx::query(sqlx::AssertSqlSafe(insert_sql.clone()))
                .bind(&batch.run_id)
                .bind(i64::try_from(entry.sequence).map_err(|_| {
                    IronCrewError::Validation("Run-event sequence exceeds PostgreSQL BIGINT".into())
                })?)
                .bind(&entry.event_type)
                .bind(payload)
                .bind(i64::try_from(entry.payload_bytes).map_err(|_| {
                    IronCrewError::Validation(
                        "Run-event payload byte count exceeds PostgreSQL BIGINT".into(),
                    )
                })?)
                .bind(&created_at)
                .bind(&expires_at)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event append failed: {error}"
                    ))
                })?;
        }
        self.update_run_event_state_after_append(tx, batch, state, accounted)
            .await
    }

    async fn update_run_event_state_after_append(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch: &RunEventAppendBatch,
        state: RunEventStateRow,
        accounted: &AccountedRunEventBatch,
    ) -> Result<()> {
        let mut journal_complete = state.journal_complete;
        let mut expected_sequence = state.latest_sequence.saturating_add(1);
        let mut latest_sequence = state.latest_sequence;
        let mut terminal_event_sequence = state.terminal_event_sequence;
        for entry in &accounted.entries {
            if entry.sequence != expected_sequence {
                journal_complete = false;
            }
            latest_sequence = latest_sequence.max(entry.sequence);
            expected_sequence = entry.sequence.saturating_add(1);
            if entry.event_type == "run_complete" {
                terminal_event_sequence = Some(
                    terminal_event_sequence
                        .unwrap_or_default()
                        .max(entry.sequence),
                );
            }
        }
        let retained_events = state
            .retained_events
            .checked_add(accounted.event_count)
            .ok_or_else(|| IronCrewError::Validation("Run-event retained count overflow".into()))?;
        let retained_bytes = state
            .retained_bytes
            .checked_add(accounted.byte_count)
            .ok_or_else(|| IronCrewError::Validation("Run-event retained bytes overflow".into()))?;
        let sql = format!(
            "UPDATE {} SET latest_sequence = $1, retained_events = $2, \
                 retained_bytes = $3, journal_complete = $4, \
                 terminal_event_sequence = $5, updated_at = clock_timestamp() \
             WHERE run_id = $6",
            self.run_event_state_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(i64::try_from(latest_sequence).map_err(|_| {
                IronCrewError::Validation("Run-event latest sequence exceeds BIGINT".into())
            })?)
            .bind(i64::try_from(retained_events).map_err(|_| {
                IronCrewError::Validation("Run-event retained count exceeds BIGINT".into())
            })?)
            .bind(i64::try_from(retained_bytes).map_err(|_| {
                IronCrewError::Validation("Run-event retained bytes exceeds BIGINT".into())
            })?)
            .bind(journal_complete)
            .bind(
                terminal_event_sequence
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| {
                        IronCrewError::Validation(
                            "Run-event terminal sequence exceeds BIGINT".into(),
                        )
                    })?,
            )
            .bind(&batch.run_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event state append update failed: {error}"
                ))
            })?;
        Ok(())
    }
}

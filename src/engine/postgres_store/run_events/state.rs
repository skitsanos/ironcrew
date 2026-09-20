use crate::engine::run_event_timing::RunEventWriteTiming;
use crate::engine::run_events::RunEventBounds;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::{nonnegative_u64, parse_run_event_gap_reason};
use super::types::{RunEventStateDbRow, RunEventStateRow, RunEventUsageRow};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn configure_run_event_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        let timing = RunEventWriteTiming::checked(self.run_event_journal_config.write_timeout)
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL run-event write timing configuration is invalid".into(),
                )
            })?;
        let timeout_value = format!("{}ms", timing.database_timeout().as_millis());
        sqlx::query(
            "SELECT set_config('lock_timeout', $1, true), \
                    set_config('statement_timeout', $1, true)",
        )
        .bind(timeout_value)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run-event transaction timeout configuration failed: {error}"
            ))
        })?;
        Ok(())
    }

    pub(in crate::engine::postgres_store) async fn lock_run_event_usage(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<RunEventUsageRow> {
        let sql = format!(
            "SELECT retained_events, retained_bytes FROM {} \
             WHERE singleton = TRUE FOR UPDATE",
            self.run_event_usage_table
        );
        let row: Option<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event global accounting lock failed: {error}"
                ))
            })?;
        let (retained_events, retained_bytes) = row.ok_or_else(|| {
            IronCrewError::Validation(
                "PostgreSQL run-event global accounting row is missing".into(),
            )
        })?;
        Ok(RunEventUsageRow {
            retained_events: nonnegative_u64("global retained event count", retained_events)?,
            retained_bytes: nonnegative_u64("global retained byte count", retained_bytes)?,
        })
    }

    pub(in crate::engine::postgres_store) async fn run_event_state_for_update(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
    ) -> Result<Option<RunEventStateRow>> {
        self.run_event_state(tx, run_id, "FOR UPDATE").await
    }

    pub(in crate::engine::postgres_store) async fn run_event_state_for_share(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
    ) -> Result<Option<RunEventStateRow>> {
        self.run_event_state(tx, run_id, "FOR SHARE").await
    }

    async fn run_event_state(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        lock: &str,
    ) -> Result<Option<RunEventStateRow>> {
        let sql = format!(
            "SELECT flow, owner_instance_id, latest_sequence, dropped_through, \
                    retained_events, retained_bytes, journal_complete, \
                    eviction_reason, terminal_event_sequence \
             FROM {} WHERE run_id = $1 {lock}",
            self.run_event_state_table,
        );
        let row: Option<RunEventStateDbRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event state lookup failed: {error}"
                ))
            })?;
        row.map(
            |(
                flow,
                owner_instance_id,
                latest_sequence,
                dropped_through,
                retained_events,
                retained_bytes,
                journal_complete,
                eviction_reason,
                terminal_event_sequence,
            )| {
                Ok(RunEventStateRow {
                    flow,
                    owner_instance_id,
                    latest_sequence: nonnegative_u64("latest sequence", latest_sequence)?,
                    dropped_through: nonnegative_u64("dropped boundary", dropped_through)?,
                    retained_events: nonnegative_u64(
                        "per-run retained event count",
                        retained_events,
                    )?,
                    retained_bytes: nonnegative_u64("per-run retained byte count", retained_bytes)?,
                    journal_complete,
                    eviction_reason: eviction_reason
                        .as_deref()
                        .map(parse_run_event_gap_reason)
                        .transpose()?,
                    terminal_event_sequence: terminal_event_sequence
                        .map(|value| nonnegative_u64("terminal event sequence", value))
                        .transpose()?,
                })
            },
        )
        .transpose()
    }

    pub(in crate::engine::postgres_store) async fn run_event_bounds(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        run_id: &str,
        state: &RunEventStateRow,
    ) -> Result<RunEventBounds> {
        let sql = format!(
            "SELECT MIN(sequence) FROM {} WHERE run_id = $1",
            self.run_events_table
        );
        let earliest: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event earliest-bound lookup failed: {error}"
                ))
            })?;
        let bounds = RunEventBounds {
            earliest_retained_sequence: earliest
                .map(|value| nonnegative_u64("earliest retained sequence", value))
                .transpose()?,
            latest_sequence: state.latest_sequence,
            dropped_through: state.dropped_through,
            retained_events: state.retained_events,
            retained_bytes: state.retained_bytes,
            journal_complete: state.journal_complete,
        };
        bounds.validate()?;
        Ok(bounds)
    }
}

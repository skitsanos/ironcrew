use std::collections::HashMap;

use sqlx::Row;

use crate::engine::run_events::{RunEventAppendBatch, RunEventAppendEntry};
use crate::engine::run_history::RunStatus;
use crate::utils::error::{IronCrewError, Result};

use super::super::super::PostgresStore;
use super::super::super::codecs::{decode_stored_json, nonnegative_u64};
use super::super::{RunEventAppendTarget, RunEventStateRow};

type ExistingRunEvents = HashMap<u64, (String, serde_json::Value, u64)>;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn lock_run_event_append_target(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch: &RunEventAppendBatch,
    ) -> Result<RunEventAppendTarget> {
        let run_sql = format!(
            "SELECT flow, owner_instance_id, status, \
                    CASE WHEN lease_expires_at = '' THEN FALSE ELSE \
                        lease_expires_at::timestamptz > clock_timestamp() \
                    END AS lease_active, \
                    duration_ms, CASE WHEN octet_length(usage::text) <= 4096 THEN usage ELSE NULL END AS usage \
             FROM {} WHERE run_id = $1 FOR UPDATE",
            self.table_name
        );
        let run: Option<(
            String,
            String,
            String,
            bool,
            i64,
            sqlx::types::Json<crate::usage::UsageSnapshot>,
        )> = sqlx::query_as(sqlx::AssertSqlSafe(run_sql))
            .bind(&batch.run_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event run fence lookup failed: {error}"
                ))
            })?;
        let (flow, owner, status, lease_active, duration_ms, usage) = run.ok_or_else(|| {
            IronCrewError::Validation(format!("Run '{}' not found", batch.run_id))
        })?;
        if flow != batch.flow {
            return Err(IronCrewError::Conflict(format!(
                "Run-event flow '{}' does not match run '{}'",
                batch.flow, batch.run_id
            )));
        }
        if owner != batch.owner_instance_id {
            return Err(IronCrewError::Conflict(format!(
                "Run '{}' is owned by instance '{}', not '{}'",
                batch.run_id, owner, batch.owner_instance_id
            )));
        }
        Ok(RunEventAppendTarget {
            status: status.parse::<RunStatus>()?,
            lease_active,
            duration_ms,
            usage: usage.0,
        })
    }

    pub(in crate::engine::postgres_store) async fn initialize_run_event_state(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch: &RunEventAppendBatch,
    ) -> Result<RunEventStateRow> {
        let sql = format!(
            "INSERT INTO {} (run_id, flow, owner_instance_id) \
             VALUES ($1, $2, $3) ON CONFLICT (run_id) DO NOTHING",
            self.run_event_state_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&batch.run_id)
            .bind(&batch.flow)
            .bind(&batch.owner_instance_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event state initialization failed: {error}"
                ))
            })?;
        let state = self
            .run_event_state_for_update(tx, &batch.run_id)
            .await?
            .ok_or_else(|| {
                IronCrewError::Validation("PostgreSQL run-event state is missing".into())
            })?;
        if state.flow != batch.flow || state.owner_instance_id != batch.owner_instance_id {
            return Err(IronCrewError::Conflict(format!(
                "Run-event state fence for '{}' does not match the current run owner/flow",
                batch.run_id
            )));
        }
        Ok(state)
    }

    pub(in crate::engine::postgres_store) async fn load_existing_run_events(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        batch: &RunEventAppendBatch,
        sequence_values: &[i64],
    ) -> Result<ExistingRunEvents> {
        let sql = format!(
            "SELECT sequence, event_type, payload::text AS payload, payload_bytes \
             FROM {} WHERE run_id = $1 AND sequence = ANY($2::bigint[])",
            self.run_events_table
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&batch.run_id)
            .bind(sequence_values)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event duplicate lookup failed: {error}"
                ))
            })?;
        let mut existing = HashMap::with_capacity(rows.len());
        for row in rows {
            let sequence = nonnegative_u64(
                "stored sequence",
                row.try_get("sequence").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                })?,
            )?;
            let event_type = row.try_get("event_type").map_err(|error| {
                IronCrewError::Validation(format!("Run-event event_type column: {error}"))
            })?;
            let payload_raw: String = row.try_get("payload").map_err(|error| {
                IronCrewError::Validation(format!("Run-event payload column: {error}"))
            })?;
            let payload = decode_stored_json(&payload_raw, "run_events.payload")?;
            let payload_bytes = nonnegative_u64(
                "stored payload byte count",
                row.try_get("payload_bytes").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event payload_bytes column: {error}"))
                })?,
            )?;
            existing.insert(sequence, (event_type, payload, payload_bytes));
        }
        Ok(existing)
    }

    pub(in crate::engine::postgres_store) fn partition_new_run_event_entries(
        batch: &RunEventAppendBatch,
        state: &RunEventStateRow,
        existing: &ExistingRunEvents,
    ) -> Result<(u64, Vec<RunEventAppendEntry>)> {
        let mut duplicate_events = 0u64;
        let mut new_entries = Vec::new();
        for entry in &batch.entries {
            match existing.get(&entry.sequence) {
                Some((event_type, payload, payload_bytes)) => {
                    if entry.sequence > state.latest_sequence {
                        return Err(IronCrewError::Conflict(format!(
                            "Run-event sequence {} for '{}' exists beyond the journal state boundary",
                            entry.sequence, batch.run_id
                        )));
                    }
                    let expected_bytes = u64::try_from(entry.payload_bytes).map_err(|_| {
                        IronCrewError::Validation(
                            "Run-event payload byte count exceeds BIGINT".into(),
                        )
                    })?;
                    if event_type != &entry.event_type
                        || payload != &entry.payload
                        || *payload_bytes != expected_bytes
                    {
                        return Err(IronCrewError::Conflict(format!(
                            "Run-event sequence {} for '{}' already contains different data",
                            entry.sequence, batch.run_id
                        )));
                    }
                    duplicate_events = duplicate_events.saturating_add(1);
                }
                None if entry.sequence <= state.latest_sequence => {
                    return Err(IronCrewError::Conflict(format!(
                        "Run-event sequence {} for '{}' was already allocated and is no longer retained",
                        entry.sequence, batch.run_id
                    )));
                }
                None => new_entries.push(entry.clone()),
            }
        }
        Ok((duplicate_events, new_entries))
    }

    pub(in crate::engine::postgres_store) fn validate_new_run_event_entries(
        batch: &RunEventAppendBatch,
        target: &RunEventAppendTarget,
        state: &RunEventStateRow,
        new_entries: &[RunEventAppendEntry],
    ) -> Result<()> {
        if new_entries.is_empty() {
            return Ok(());
        }
        if state.terminal_event_sequence.is_some() {
            return Err(IronCrewError::Conflict(format!(
                "Run-event journal for '{}' is sealed after run_complete",
                batch.run_id
            )));
        }
        if target.status.is_in_flight() {
            if !target.lease_active {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{}' no longer has an active owner lease",
                    batch.run_id
                )));
            }
            if new_entries
                .iter()
                .any(|entry| entry.event_type == "run_complete")
            {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{}' cannot append run_complete before its terminal record",
                    batch.run_id
                )));
            }
            return Ok(());
        }
        if matches!(&target.status, RunStatus::Abandoned) {
            return Err(IronCrewError::Conflict(format!(
                "Abandoned run '{}' cannot append terminal journal events",
                batch.run_id
            )));
        }
        let [terminal_entry] = new_entries else {
            return Err(IronCrewError::Conflict(format!(
                "Terminal run '{}' accepts exactly one new run_complete event",
                batch.run_id
            )));
        };
        if terminal_entry.event_type != "run_complete" {
            return Err(IronCrewError::Conflict(format!(
                "Terminal run '{}' cannot append a nonterminal journal event",
                batch.run_id
            )));
        }
        let expected_duration_ms = nonnegative_u64("terminal duration", target.duration_ms)?;
        let expected_status = target.status.to_string();
        let terminal_data = terminal_entry
            .payload
            .get("data")
            .and_then(serde_json::Value::as_object);
        let terminal_matches = terminal_data.is_some_and(|data| {
            data.get("run_id").and_then(serde_json::Value::as_str) == Some(batch.run_id.as_str())
                && data.get("status").and_then(serde_json::Value::as_str)
                    == Some(expected_status.as_str())
                && data.get("duration_ms").and_then(serde_json::Value::as_u64)
                    == Some(expected_duration_ms)
                && data
                    .get("usage")
                    .and_then(|value| {
                        serde_json::from_value::<crate::usage::UsageSnapshot>(value.clone()).ok()
                    })
                    .as_ref()
                    == Some(&target.usage)
        });
        if !terminal_matches {
            return Err(IronCrewError::Conflict(format!(
                "run_complete for '{}' does not match its terminal run record",
                batch.run_id
            )));
        }
        Ok(())
    }
}

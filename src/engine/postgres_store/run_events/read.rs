use sqlx::Row;

use crate::engine::run_events::{
    RunEventBounds, RunEventEntry, RunEventGap, RunEventGapReason, RunEventPage,
    RunEventTerminalState,
};
use crate::engine::run_history::RunStatus;
use crate::utils::error::{IronCrewError, Result};

use super::super::codecs::{decode_stored_json, nonnegative_u64};
use super::super::{PostgresStore, validate_human_input_route};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn read_run_events_journal(
        &self,
        flow: &str,
        run_id: &str,
        after_sequence: u64,
    ) -> Result<RunEventPage> {
        validate_human_input_route("flow", flow, 255)?;
        validate_human_input_route("run id", run_id, 128)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL run-event read transaction failed: {error}"
            ))
        })?;
        let run_sql = format!(
            "SELECT flow, status, duration_ms, total_tokens, \
                    to_char(clock_timestamp() AT TIME ZONE 'UTC', \
                        'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS snapshot_at \
             FROM {} \
             WHERE run_id = $1 FOR SHARE",
            self.table_name
        );
        let run: Option<(String, String, i64, i32, String)> =
            sqlx::query_as(sqlx::AssertSqlSafe(run_sql))
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL run-event run lookup failed: {error}"
                    ))
                })?;
        let (stored_flow, status, duration_ms, total_tokens, snapshot_at) =
            run.ok_or_else(|| IronCrewError::Validation(format!("Run '{run_id}' not found")))?;
        if stored_flow != flow {
            return Err(IronCrewError::Conflict(format!(
                "Run-event flow '{flow}' does not match run '{run_id}'"
            )));
        }
        // SSE polls are a high-frequency, multi-replica read path. A shared
        // state lock yields a consistent per-run snapshot without taking the
        // singleton accounting lock or doing retention writes. Logical
        // retention is applied below against one captured database timestamp;
        // append/reconciliation perform bounded physical cleanup.
        let state = self.run_event_state_for_share(&mut tx, run_id).await?;
        let (mut bounds, logical_gap_reason) = match &state {
            Some(state) => {
                let logical_bounds_sql = format!(
                    "SELECT \
                         MIN(sequence) FILTER (WHERE expires_at > $2::timestamptz), \
                         COUNT(*) FILTER (WHERE expires_at > $2::timestamptz)::BIGINT, \
                         COALESCE(SUM(accounted_bytes) FILTER (\
                             WHERE expires_at > $2::timestamptz), 0)::BIGINT, \
                         MAX(sequence) FILTER (WHERE expires_at <= $2::timestamptz) \
                     FROM {} WHERE run_id = $1",
                    self.run_events_table
                );
                let (earliest, retained_events, retained_bytes, latest_expired): (
                    Option<i64>,
                    i64,
                    i64,
                    Option<i64>,
                ) = sqlx::query_as(sqlx::AssertSqlSafe(logical_bounds_sql))
                    .bind(run_id)
                    .bind(&snapshot_at)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL logical run-event bounds lookup failed: {error}"
                        ))
                    })?;
                let retained_events =
                    nonnegative_u64("logical per-run retained event count", retained_events)?;
                let retained_bytes =
                    nonnegative_u64("logical per-run retained byte count", retained_bytes)?;
                let earliest_retained_sequence = earliest
                    .map(|value| nonnegative_u64("earliest retained sequence", value))
                    .transpose()?;
                let latest_expired = latest_expired
                    .map(|value| nonnegative_u64("latest expired sequence", value))
                    .transpose()?;
                let retention_boundary = match (latest_expired, earliest_retained_sequence) {
                    (Some(expired), Some(earliest)) => expired.min(earliest.saturating_sub(1)),
                    (Some(expired), None) => expired,
                    (None, _) => state.dropped_through,
                };
                let dropped_through = state.dropped_through.max(retention_boundary);
                let gap_reason = if dropped_through > state.dropped_through {
                    Some(RunEventGapReason::Retention)
                } else {
                    state.eviction_reason
                };
                let bounds = RunEventBounds {
                    earliest_retained_sequence,
                    latest_sequence: state.latest_sequence,
                    dropped_through,
                    retained_events,
                    retained_bytes,
                    journal_complete: state.journal_complete,
                };
                bounds.validate()?;
                (bounds, gap_reason)
            }
            None => (RunEventBounds::empty(), None),
        };
        if after_sequence > bounds.latest_sequence {
            return Err(IronCrewError::Validation(format!(
                "Run-event page starts ahead of latest sequence {}",
                bounds.latest_sequence
            )));
        }

        let effective_after = after_sequence.max(bounds.dropped_through);
        let event_sql = format!(
            "WITH candidate_sizes AS MATERIALIZED (\
                 SELECT sequence, accounted_bytes \
                 FROM {events} WHERE run_id = $1 AND sequence > $2 \
                   AND expires_at > $3::timestamptz \
                 ORDER BY sequence LIMIT $4\
             ), bounded_sequences AS (\
                 SELECT sequence, \
                        ROW_NUMBER() OVER (ORDER BY sequence) AS page_row, \
                        SUM(accounted_bytes) OVER (ORDER BY sequence \
                            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS page_bytes \
                 FROM candidate_sizes\
             ) \
             SELECT event.sequence, event.event_type, event.payload::text AS payload, \
                    event.payload_bytes, \
                    to_char(event.created_at AT TIME ZONE 'UTC', \
                        'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS created_at \
             FROM bounded_sequences AS bounded \
             JOIN {events} AS event ON event.run_id = $1 \
                  AND event.sequence = bounded.sequence \
             WHERE bounded.page_bytes <= $5 OR bounded.page_row = 1 \
             ORDER BY event.sequence",
            events = self.run_events_table,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(event_sql))
            .bind(run_id)
            .bind(i64::try_from(effective_after).map_err(|_| {
                IronCrewError::Validation("Run-event page boundary exceeds BIGINT".into())
            })?)
            .bind(&snapshot_at)
            .bind(
                i64::try_from(self.run_event_journal_config.page_max_events).map_err(|_| {
                    IronCrewError::Validation("Run-event page limit exceeds BIGINT".into())
                })?,
            )
            .bind(
                i64::try_from(self.run_event_journal_config.page_max_bytes).map_err(|_| {
                    IronCrewError::Validation("Run-event page byte limit exceeds BIGINT".into())
                })?,
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL run-event page lookup failed: {error}"
                ))
            })?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let payload_bytes_i64: i64 = row.try_get("payload_bytes").map_err(|error| {
                IronCrewError::Validation(format!("Run-event payload_bytes column: {error}"))
            })?;
            let payload_bytes_u64 =
                nonnegative_u64("stored page payload byte count", payload_bytes_i64)?;
            let payload_bytes = usize::try_from(payload_bytes_u64).map_err(|_| {
                IronCrewError::Validation(
                    "Run-event stored payload byte count exceeds usize".into(),
                )
            })?;
            let payload_raw: String = row.try_get("payload").map_err(|error| {
                IronCrewError::Validation(format!("Run-event payload column: {error}"))
            })?;
            events.push(RunEventEntry {
                sequence: nonnegative_u64(
                    "page sequence",
                    row.try_get("sequence").map_err(|error| {
                        IronCrewError::Validation(format!("Run-event sequence column: {error}"))
                    })?,
                )?,
                event_type: row.try_get("event_type").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event event_type column: {error}"))
                })?,
                payload: decode_stored_json(&payload_raw, "run_events.payload")?,
                payload_bytes,
                created_at: row.try_get("created_at").map_err(|error| {
                    IronCrewError::Validation(format!("Run-event created_at column: {error}"))
                })?,
            });
        }

        let mut gap = if after_sequence < bounds.dropped_through {
            let reason = logical_gap_reason.ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL run-event dropped boundary has no reason".into(),
                )
            })?;
            // `RunEventPage` can describe one gap. Return this prefix gap by
            // itself so the caller advances to `dropped_through`; the next
            // read can then report a distinct internal writer gap without
            // emitting non-monotonic SSE ids.
            events.clear();
            Some(RunEventGap {
                first_sequence: after_sequence.saturating_add(1),
                last_sequence: bounds.dropped_through,
                reason,
            })
        } else {
            None
        };
        if gap.is_none() {
            let mut expected = after_sequence.saturating_add(1);
            let mut internal_gap = None;
            for (index, event) in events.iter().enumerate() {
                if event.sequence > expected {
                    internal_gap = Some((index, expected, event.sequence.saturating_sub(1)));
                    break;
                }
                expected = event.sequence.saturating_add(1);
            }
            if let Some((index, first_sequence, last_sequence)) = internal_gap {
                if index == 0 {
                    events.clear();
                    gap = Some(RunEventGap {
                        first_sequence,
                        last_sequence,
                        reason: RunEventGapReason::WriterBackpressure,
                    });
                } else {
                    // Return only the contiguous prefix. The caller advances
                    // through it and observes the internal gap on the next
                    // read, preserving the single-gap page contract.
                    events.truncate(index);
                }
            }
            if gap.is_none() && events.is_empty() && after_sequence < bounds.latest_sequence {
                gap = Some(RunEventGap {
                    first_sequence: after_sequence.saturating_add(1),
                    last_sequence: bounds.latest_sequence,
                    reason: RunEventGapReason::WriterBackpressure,
                });
            }
        }

        let status = status.parse::<RunStatus>()?;
        let terminal_event_sequence = state
            .as_ref()
            .and_then(|state| state.terminal_event_sequence);
        if status.is_terminal() && terminal_event_sequence.is_none() {
            bounds.journal_complete = false;
        }
        let terminal = if status.is_terminal() {
            Some(RunEventTerminalState {
                status,
                duration_ms: nonnegative_u64("terminal duration", duration_ms)?,
                total_tokens: u32::try_from(total_tokens).map_err(|_| {
                    IronCrewError::Validation(
                        "PostgreSQL run-event terminal token count is negative".into(),
                    )
                })?,
                event_sequence: terminal_event_sequence,
            })
        } else {
            None
        };
        let page = RunEventPage {
            run_id: run_id.to_owned(),
            after_sequence,
            events,
            bounds,
            gap,
            terminal,
        };
        page.validate(&self.run_event_journal_config)?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PostgreSQL run-event read commit failed: {error}"))
        })?;
        Ok(page)
    }
}

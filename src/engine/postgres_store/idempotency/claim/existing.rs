use chrono::{DateTime, Utc};

use crate::engine::idempotency::{IdempotencyClaim, IdempotencyClaimOutcome, IdempotencyState};
use crate::utils::error::{IronCrewError, Result};

use super::super::super::PostgresStore;
use super::super::super::codecs::parse_timestamp;
use super::ClaimDecision;

impl PostgresStore {
    pub(super) async fn prune_idempotency_for_claim(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
        prune_batch: usize,
    ) -> Result<()> {
        if prune_batch == 0 {
            return Ok(());
        }
        let sql = format!(
            "DELETE FROM {table} WHERE key_hash IN (\
                 SELECT key_hash FROM {table} \
                 WHERE state IN ('completed', 'indeterminate') \
                   AND expires_at IS NOT NULL \
                   AND expires_at::timestamptz <= $1::timestamptz \
                 ORDER BY expires_at::timestamptz, key_hash LIMIT $2\
             )",
            table = self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(i64::try_from(prune_batch).unwrap_or(i64::MAX))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency claim pruning failed: {error}"
                ))
            })?;
        Ok(())
    }

    pub(super) async fn resolve_existing_idempotency_claim(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        claim: &IdempotencyClaim,
        database_timestamp: DateTime<Utc>,
    ) -> Result<Option<ClaimDecision>> {
        let Some(record) = self
            .get_idempotency_in_transaction(tx, &claim.key_hash)
            .await?
        else {
            return Ok(None);
        };
        let expired_terminal = record.state.is_terminal()
            && record
                .expires_at
                .as_deref()
                .map(|expires_at| {
                    parse_timestamp("stored idempotency retention expiry", expires_at)
                        .map(|expires_at| expires_at <= database_timestamp)
                })
                .transpose()?
                .unwrap_or(false);
        if expired_terminal {
            let sql = format!("DELETE FROM {} WHERE key_hash = $1", self.idempotency_table);
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(&claim.key_hash)
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL expired idempotency delete failed: {error}"
                    ))
                })?;
            return Ok(None);
        }
        let outcome = if record.principal_id != claim.principal_id
            || record.request_fingerprint != claim.request_fingerprint
        {
            IdempotencyClaimOutcome::Conflict
        } else if record.state == IdempotencyState::Indeterminate {
            IdempotencyClaimOutcome::Indeterminate(record)
        } else if record.replayable() {
            IdempotencyClaimOutcome::Replay(record)
        } else if record.state.is_in_flight()
            && parse_timestamp("stored idempotency lease expiry", &record.lease_expires_at)?
                <= database_timestamp
        {
            let record = self
                .mark_record_indeterminate_in_transaction(tx, record)
                .await?;
            IdempotencyClaimOutcome::Indeterminate(record)
        } else if record.state.is_in_flight() {
            IdempotencyClaimOutcome::InProgress(record)
        } else {
            IdempotencyClaimOutcome::Indeterminate(record)
        };
        Ok(Some(ClaimDecision {
            outcome,
            commit_error: "PostgreSQL idempotency claim commit failed",
        }))
    }
}

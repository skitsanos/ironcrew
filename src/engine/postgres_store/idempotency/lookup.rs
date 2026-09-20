use crate::engine::idempotency::{
    IdempotencyLookup, IdempotencyRecord, IdempotencyState, PrincipalId, validate_digest,
};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::{idempotency_record, parse_timestamp};
use super::types::idempotency_select_columns;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn get_idempotency_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        key_hash: &str,
    ) -> Result<Option<IdempotencyRecord>> {
        let columns = idempotency_select_columns();
        let sql = format!(
            "SELECT {columns} FROM {} WHERE key_hash = $1 FOR UPDATE",
            self.idempotency_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PostgreSQL idempotency lookup failed: {error}"))
            })?;
        row.as_ref().map(idempotency_record).transpose()
    }

    pub(in crate::engine::postgres_store) async fn idempotency_principal_for_key(
        &self,
        key_hash: &str,
    ) -> Result<Option<PrincipalId>> {
        let sql = format!(
            "SELECT principal_id FROM {} WHERE key_hash = $1",
            self.idempotency_table
        );
        let principal: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(key_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency principal lookup failed: {error}"
                ))
            })?;
        principal.map(PrincipalId::from_digest).transpose()
    }

    pub(in crate::engine::postgres_store) async fn lookup_idempotency_for_principal_record(
        &self,
        principal_id: &PrincipalId,
        key_hash: &str,
        request_fingerprint: &str,
        now: &str,
    ) -> Result<IdempotencyLookup> {
        principal_id.validate()?;
        validate_digest("idempotency key hash", key_hash)?;
        validate_digest("request fingerprint", request_fingerprint)?;
        parse_timestamp("idempotency lookup time", now)?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency lookup transaction failed: {error}"
            ))
        })?;
        self.lock_idempotency_key(&mut tx, key_hash).await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "idempotency lookup")
            .await?;
        let now_timestamp = parse_timestamp("PostgreSQL idempotency clock", &database_now)?;

        let Some(record) = self
            .get_idempotency_in_transaction(&mut tx, key_hash)
            .await?
        else {
            tx.commit().await.map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency lookup commit failed: {error}"
                ))
            })?;
            return Ok(IdempotencyLookup::Miss);
        };

        let outcome = if &record.principal_id != principal_id
            || record.request_fingerprint != request_fingerprint
        {
            IdempotencyLookup::Conflict
        } else if record.state.is_terminal()
            && record
                .expires_at
                .as_deref()
                .map(|expires_at| {
                    parse_timestamp("stored idempotency retention expiry", expires_at)
                        .map(|expires_at| expires_at <= now_timestamp)
                })
                .transpose()?
                .unwrap_or(false)
        {
            IdempotencyLookup::Miss
        } else if record.state == IdempotencyState::Indeterminate {
            IdempotencyLookup::Indeterminate(record)
        } else if record.replayable() {
            IdempotencyLookup::Replay(record)
        } else {
            match record.state {
                IdempotencyState::Claimed | IdempotencyState::Running => {
                    let lease = parse_timestamp(
                        "stored idempotency lease expiry",
                        &record.lease_expires_at,
                    )?;
                    if lease > now_timestamp {
                        IdempotencyLookup::InProgress(record)
                    } else {
                        IdempotencyLookup::Indeterminate(record)
                    }
                }
                IdempotencyState::Completed | IdempotencyState::Indeterminate => {
                    IdempotencyLookup::Indeterminate(record)
                }
            }
        };
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency lookup commit failed: {error}"
            ))
        })?;
        Ok(outcome)
    }
}

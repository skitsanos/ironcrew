use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::engine::idempotency::{IdempotencyClaim, IdempotencyClaimOutcome, PrincipalId};
use crate::utils::error::{IronCrewError, Result};

use super::super::super::PostgresStore;
use super::super::super::codecs::{idempotency_record, parse_timestamp};
use super::super::types::idempotency_select_columns;
use super::{ClaimDecision, ExclusiveScopeResolution};

impl PostgresStore {
    pub(super) async fn resolve_exclusive_scope_claim(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        claim: &IdempotencyClaim,
        database_timestamp: DateTime<Utc>,
    ) -> Result<ExclusiveScopeResolution> {
        let Some(exclusive_scope) = claim.exclusive_scope.as_deref() else {
            return Ok(ExclusiveScopeResolution::Continue(None));
        };
        let columns = idempotency_select_columns();
        let sql = format!(
            "SELECT {columns} FROM {} \
             WHERE exclusive_scope = $1 AND key_hash <> $2 \
               AND state IN ('claimed', 'running') FOR UPDATE",
            self.idempotency_table
        );
        if let Some(row) = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(exclusive_scope)
            .bind(&claim.key_hash)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency exclusive-scope lookup failed: {error}"
                ))
            })?
        {
            let record = idempotency_record(&row)?;
            let lease =
                parse_timestamp("stored idempotency lease expiry", &record.lease_expires_at)?;
            if lease > database_timestamp {
                return Ok(Self::exclusive_scope_busy(
                    "PostgreSQL idempotency busy commit failed",
                ));
            }
            self.mark_record_indeterminate_in_transaction(tx, record)
                .await?;
            return Ok(Self::exclusive_scope_busy(
                "PostgreSQL expired idempotency barrier commit failed",
            ));
        }

        let hazard_sql = format!(
            "SELECT key_hash, principal_id, completed_at FROM {} \
             WHERE exclusive_scope = $1 AND key_hash <> $2 \
               AND state = 'indeterminate' \
             ORDER BY completed_at, key_hash LIMIT 2 FOR UPDATE",
            self.idempotency_table
        );
        let hazard_rows = sqlx::query(sqlx::AssertSqlSafe(hazard_sql))
            .bind(exclusive_scope)
            .bind(&claim.key_hash)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL indeterminate exclusive-scope lookup failed: {error}"
                ))
            })?;
        if hazard_rows.is_empty() {
            return Ok(ExclusiveScopeResolution::Continue(None));
        }
        let hazard = if hazard_rows.len() == 1 {
            Some(Self::decode_recovery_hazard(&hazard_rows[0])?)
        } else {
            None
        };
        let grace = chrono::Duration::from_std(self.lease.ttl()).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL idempotency recovery grace is out of range".into(),
            )
        })?;
        let grace_elapsed = hazard
            .as_ref()
            .and_then(|(_, _, completed_at)| completed_at.as_deref())
            .map(|completed_at| {
                parse_timestamp("stored idempotency hazard completion", completed_at)
            })
            .transpose()?
            .and_then(|completed_at| completed_at.checked_add_signed(grace))
            .is_some_and(|recovery_at| recovery_at <= database_timestamp);
        let recoverable = hazard.as_ref().is_some_and(|(key_hash, principal_id, _)| {
            principal_id == &claim.principal_id
                && claim.recovery_key_hash.as_deref() == Some(key_hash.as_str())
        }) && grace_elapsed;
        if !recoverable {
            return Ok(Self::exclusive_scope_busy(
                "PostgreSQL idempotency hazard commit failed",
            ));
        }
        let recovery_key_hash = claim
            .recovery_key_hash
            .clone()
            .ok_or_else(|| IronCrewError::Validation("Missing idempotency recovery key".into()))?;
        // Keep the locked hazard bound until every quota check has passed. A
        // quota-denied transaction is committed so bounded pruning/accounting
        // can progress without consuming the recovery capability.
        Ok(ExclusiveScopeResolution::Continue(Some(recovery_key_hash)))
    }

    fn exclusive_scope_busy(commit_error: &'static str) -> ExclusiveScopeResolution {
        ExclusiveScopeResolution::Return(Box::new(ClaimDecision {
            outcome: IdempotencyClaimOutcome::Busy,
            commit_error,
        }))
    }

    fn decode_recovery_hazard(
        row: &sqlx::postgres::PgRow,
    ) -> Result<(String, PrincipalId, Option<String>)> {
        let key_hash = row.try_get("key_hash").map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency hazard key decode failed: {error}"
            ))
        })?;
        let principal_id = row
            .try_get::<String, _>("principal_id")
            .map(PrincipalId::from_digest)
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency hazard principal decode failed: {error}"
                ))
            })??;
        let completed_at = row.try_get("completed_at").map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency hazard timestamp decode failed: {error}"
            ))
        })?;
        Ok((key_hash, principal_id, completed_at))
    }
}

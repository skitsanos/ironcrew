use crate::engine::idempotency::{
    IdempotencyClaim, IdempotencyClaimOutcome, IdempotencyLimits, IdempotencyQuotaResource,
    IdempotencyQuotaScope,
};
use crate::utils::error::{IronCrewError, Result};

use super::super::super::PostgresStore;
use super::super::types::IDEMPOTENCY_COLUMNS;
use super::ClaimDecision;

impl PostgresStore {
    pub(super) async fn persist_idempotency_claim(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        claim: IdempotencyClaim,
        limits: IdempotencyLimits,
        database_now: String,
        lease_expires_at: String,
        recovery_hazard_key: Option<&str>,
    ) -> Result<ClaimDecision> {
        let (global_usage, principal_usage) = self
            .idempotency_accounting_for_update(tx, &claim.principal_id)
            .await?;
        let quota = if global_usage.records >= limits.global_max_records {
            Some((
                IdempotencyQuotaScope::Global,
                IdempotencyQuotaResource::Records,
                None,
            ))
        } else if principal_usage.records >= limits.principal_max_records {
            Some((
                IdempotencyQuotaScope::Principal,
                IdempotencyQuotaResource::Records,
                Some(&claim.principal_id),
            ))
        } else if principal_usage.in_flight >= limits.principal_max_in_flight {
            Some((
                IdempotencyQuotaScope::Principal,
                IdempotencyQuotaResource::InFlight,
                Some(&claim.principal_id),
            ))
        } else {
            None
        };
        if let Some((scope, resource, retry_principal)) = quota {
            let retry_after_seconds = self
                .idempotency_retry_after_seconds(tx, retry_principal, resource, &database_now)
                .await?;
            return Ok(ClaimDecision {
                outcome: IdempotencyClaimOutcome::QuotaExceeded {
                    scope,
                    resource,
                    retry_after_seconds,
                },
                commit_error: "PostgreSQL idempotency quota commit failed",
            });
        }

        let mut record = claim.to_record();
        record.lease_expires_at = lease_expires_at;
        record.created_at = database_now.clone();
        record.updated_at = database_now;
        if let Some(response_body) = record.response_body.as_ref() {
            let response_fits = global_usage
                .response_bytes
                .checked_add(response_body.len())
                .is_some_and(|total| total <= limits.global_max_response_bytes)
                && principal_usage
                    .response_bytes
                    .checked_add(response_body.len())
                    .is_some_and(|total| total <= limits.principal_max_response_bytes);
            if !response_fits {
                record.response_body = None;
            }
        }
        record.validate()?;
        if let Some(hazard_key) = recovery_hazard_key {
            let clear_sql = format!(
                "UPDATE {} SET exclusive_scope = NULL, updated_at = $1 \
                 WHERE key_hash = $2 AND exclusive_scope = $3 \
                   AND principal_id = $4 AND state = 'indeterminate'",
                self.idempotency_table
            );
            let cleared = sqlx::query(sqlx::AssertSqlSafe(clear_sql))
                .bind(&record.created_at)
                .bind(hazard_key)
                .bind(record.exclusive_scope.as_deref())
                .bind(record.principal_id.as_str())
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL idempotency hazard recovery failed: {error}"
                    ))
                })?;
            if cleared.rows_affected() != 1 {
                return Err(IronCrewError::Conflict(
                    "Idempotency recovery hazard changed before claim insertion".into(),
                ));
            }
        }
        let base_revision = record
            .base_revision
            .map(i64::try_from)
            .transpose()
            .map_err(|_| {
                IronCrewError::Validation(
                    "Idempotency base revision is out of PostgreSQL range".into(),
                )
            })?;
        let sql = format!(
            "INSERT INTO {} ({IDEMPOTENCY_COLUMNS}) VALUES(\
                 $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'claimed', \
                 $11, $12, $13, $14, $14, NULL, NULL, $15\
             )",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&record.key_hash)
            .bind(record.principal_id.as_str())
            .bind(&record.request_fingerprint)
            .bind(&record.operation)
            .bind(&record.scope)
            .bind(&record.resource_id)
            .bind(&record.exclusive_scope)
            .bind(&record.attempt_id)
            .bind(&record.owner_instance_id)
            .bind(base_revision)
            .bind(record.response_status.map(i32::from))
            .bind(&record.response_body)
            .bind(&record.lease_expires_at)
            .bind(&record.created_at)
            .bind(i64::try_from(record.ttl_seconds).map_err(|_| {
                IronCrewError::Validation("Idempotency TTL is out of PostgreSQL range".into())
            })?)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency claim insert failed: {error}"
                ))
            })?;
        Ok(ClaimDecision {
            outcome: IdempotencyClaimOutcome::Claimed(record),
            commit_error: "PostgreSQL idempotency claim commit failed",
        })
    }
}

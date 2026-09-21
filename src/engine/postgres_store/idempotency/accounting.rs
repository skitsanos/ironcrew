use crate::engine::idempotency::{IdempotencyQuotaResource, PrincipalId};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::decode_idempotency_accounting;
use super::types::IdempotencyAccounting;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn idempotency_accounting_for_update(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: &PrincipalId,
    ) -> Result<(IdempotencyAccounting, IdempotencyAccounting)> {
        let global_sql = format!(
            "SELECT record_count, in_flight_count, response_bytes FROM {} \
             WHERE principal_id = 'global' AND is_global = TRUE FOR UPDATE",
            self.idempotency_accounting_table
        );
        let global = sqlx::query_as(sqlx::AssertSqlSafe(global_sql))
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL global idempotency accounting lookup failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency accounting row is missing".into(),
                )
            })?;

        let principal_sql = format!(
            "SELECT record_count, in_flight_count, response_bytes FROM {} \
             WHERE principal_id = $1 AND is_global = FALSE FOR UPDATE",
            self.idempotency_accounting_table
        );
        let principal = sqlx::query_as(sqlx::AssertSqlSafe(principal_sql))
            .bind(principal_id.as_str())
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL principal idempotency accounting lookup failed: {error}"
                ))
            })?;
        Ok((
            decode_idempotency_accounting(global)?,
            principal
                .map(decode_idempotency_accounting)
                .transpose()?
                .unwrap_or(IdempotencyAccounting {
                    records: 0,
                    in_flight: 0,
                    response_bytes: 0,
                }),
        ))
    }

    pub(in crate::engine::postgres_store) async fn idempotency_retry_after_seconds(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        principal_id: Option<&PrincipalId>,
        resource: IdempotencyQuotaResource,
        database_now: &str,
    ) -> Result<u64> {
        let principal = principal_id.map(PrincipalId::as_str);
        let sql = match resource {
            IdempotencyQuotaResource::Records => format!(
                "SELECT GREATEST(1, COALESCE(CEIL(EXTRACT(EPOCH FROM (MIN(\
                     CASE WHEN state IN ('completed', 'indeterminate') \
                          THEN expires_at::timestamptz \
                          ELSE lease_expires_at::timestamptz + \
                               ttl_seconds * interval '1 second' END\
                 ) - $1::timestamptz)))::BIGINT, 1)) \
                 FROM {} WHERE ($2::TEXT IS NULL OR principal_id = $2)",
                self.idempotency_table
            ),
            IdempotencyQuotaResource::InFlight => format!(
                "SELECT GREATEST(1, COALESCE(CEIL(EXTRACT(EPOCH FROM (\
                     MIN(lease_expires_at::timestamptz) - $1::timestamptz\
                 )))::BIGINT, 1)) \
                 FROM {} WHERE state IN ('claimed', 'running') \
                   AND ($2::TEXT IS NULL OR principal_id = $2)",
                self.idempotency_table
            ),
        };
        let seconds: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(principal)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency quota retry calculation failed: {error}"
                ))
            })?;
        u64::try_from(seconds).map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL idempotency quota retry delay is out of range".into(),
            )
        })
    }
}

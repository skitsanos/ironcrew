use crate::engine::idempotency::{IdempotencyLimits, IdempotencyUsage, PrincipalId};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::accounting_row_value;
impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn idempotency_usage_record(
        &self,
        principal_id: &PrincipalId,
        limits: IdempotencyLimits,
    ) -> Result<IdempotencyUsage> {
        principal_id.validate()?;
        limits.validate()?;
        let threshold = |limit: usize, percentage: usize| {
            i64::try_from(limit.saturating_mul(percentage).div_ceil(100).max(1)).unwrap_or(i64::MAX)
        };
        let sql = format!(
            "SELECT \
                 global.record_count AS global_records, \
                 global.in_flight_count AS global_in_flight, \
                 global.response_bytes AS global_response_bytes, \
                 COALESCE(principal.record_count, 0) AS principal_records, \
                 COALESCE(principal.in_flight_count, 0) AS principal_in_flight, \
                 COALESCE(principal.response_bytes, 0) AS principal_response_bytes, \
                 stats.principal_count, stats.max_principal_records, \
                 stats.max_principal_in_flight, stats.max_principal_response_bytes, \
                 stats.at_80, stats.at_90, stats.at_100 \
             FROM {accounting} AS global \
             LEFT JOIN {accounting} AS principal \
               ON principal.principal_id = $1 AND principal.is_global = FALSE \
             CROSS JOIN LATERAL (\
                 SELECT COUNT(*)::BIGINT AS principal_count, \
                        COALESCE(MAX(record_count), 0)::BIGINT AS max_principal_records, \
                        COALESCE(MAX(in_flight_count), 0)::BIGINT AS max_principal_in_flight, \
                        COALESCE(MAX(response_bytes), 0)::BIGINT AS max_principal_response_bytes, \
                        COUNT(*) FILTER (WHERE record_count >= $2 OR in_flight_count >= $3 \
                                                OR response_bytes >= $4)::BIGINT AS at_80, \
                        COUNT(*) FILTER (WHERE record_count >= $5 OR in_flight_count >= $6 \
                                                OR response_bytes >= $7)::BIGINT AS at_90, \
                        COUNT(*) FILTER (WHERE record_count >= $8 OR in_flight_count >= $9 \
                                                OR response_bytes >= $10)::BIGINT AS at_100 \
                 FROM {accounting} WHERE is_global = FALSE\
             ) AS stats \
             WHERE global.principal_id = 'global' AND global.is_global = TRUE",
            accounting = self.idempotency_accounting_table
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(principal_id.as_str())
            .bind(threshold(limits.principal_max_records, 80))
            .bind(threshold(limits.principal_max_in_flight, 80))
            .bind(threshold(limits.principal_max_response_bytes, 80))
            .bind(threshold(limits.principal_max_records, 90))
            .bind(threshold(limits.principal_max_in_flight, 90))
            .bind(threshold(limits.principal_max_response_bytes, 90))
            .bind(threshold(limits.principal_max_records, 100))
            .bind(threshold(limits.principal_max_in_flight, 100))
            .bind(threshold(limits.principal_max_response_bytes, 100))
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL idempotency usage query failed: {error}"
                ))
            })?
            .ok_or_else(|| {
                IronCrewError::Validation(
                    "PostgreSQL global idempotency accounting row is missing".into(),
                )
            })?;
        Ok(IdempotencyUsage {
            global_records: accounting_row_value(&row, "global_records")?,
            global_in_flight: accounting_row_value(&row, "global_in_flight")?,
            global_response_bytes: accounting_row_value(&row, "global_response_bytes")?,
            principal_records: accounting_row_value(&row, "principal_records")?,
            principal_in_flight: accounting_row_value(&row, "principal_in_flight")?,
            principal_response_bytes: accounting_row_value(&row, "principal_response_bytes")?,
            principal_count: accounting_row_value(&row, "principal_count")?,
            max_principal_records: accounting_row_value(&row, "max_principal_records")?,
            max_principal_in_flight: accounting_row_value(&row, "max_principal_in_flight")?,
            max_principal_response_bytes: accounting_row_value(
                &row,
                "max_principal_response_bytes",
            )?,
            principals_at_or_above_80_percent: accounting_row_value(&row, "at_80")?,
            principals_at_or_above_90_percent: accounting_row_value(&row, "at_90")?,
            principals_at_or_above_100_percent: accounting_row_value(&row, "at_100")?,
        })
    }

    // ─── Persistent sessions ────────────────────────────────────────────────
}

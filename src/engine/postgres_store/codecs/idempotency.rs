use sqlx::{Row, postgres::PgRow};

use crate::engine::idempotency::{
    HARD_IDEMPOTENCY_RESPONSE_BYTES, IdempotencyRecord, IdempotencyState, PrincipalId,
};
use crate::utils::error::{IronCrewError, Result};

use super::super::idempotency::IdempotencyAccounting;
use super::canonical_timestamp;

pub(in crate::engine::postgres_store) fn decode_idempotency_accounting(
    values: (i64, i64, i64),
) -> Result<IdempotencyAccounting> {
    Ok(IdempotencyAccounting {
        records: nonnegative_accounting_value("record_count", values.0)?,
        in_flight: nonnegative_accounting_value("in_flight_count", values.1)?,
        response_bytes: nonnegative_accounting_value("response_bytes", values.2)?,
    })
}

pub(in crate::engine::postgres_store) fn accounting_row_value(
    row: &PgRow,
    column: &str,
) -> Result<usize> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|error| IronCrewError::Validation(format!("Column error: {error}")))?;
    nonnegative_accounting_value(column, value)
}

pub(in crate::engine::postgres_store) fn idempotency_record(
    row: &PgRow,
) -> Result<IdempotencyRecord> {
    let response_body_bytes = row
        .try_get::<i64, _>("response_body_bytes")
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "PostgreSQL idempotency response byte-count decode failed: {error}"
            ))
        })?;
    let response_body_bytes = usize::try_from(response_body_bytes).map_err(|_| {
        IronCrewError::Validation(
            "PostgreSQL idempotency response byte count is out of range".into(),
        )
    })?;
    if response_body_bytes > HARD_IDEMPOTENCY_RESPONSE_BYTES {
        return Err(IronCrewError::Validation(
            "Stored idempotency response body exceeds the hard byte limit".into(),
        ));
    }
    let base_revision = row
        .try_get::<Option<i64>, _>("base_revision")
        .map_err(column_error)?
        .map(u64::try_from)
        .transpose()
        .map_err(|_| {
            IronCrewError::Validation("PostgreSQL idempotency base_revision is negative".into())
        })?;
    let response_status = row
        .try_get::<Option<i32>, _>("response_status")
        .map_err(column_error)?
        .map(u16::try_from)
        .transpose()
        .map_err(|_| {
            IronCrewError::Validation(
                "PostgreSQL idempotency response_status is out of range".into(),
            )
        })?;
    let ttl_seconds = u64::try_from(row.try_get::<i64, _>("ttl_seconds").map_err(column_error)?)
        .map_err(|_| {
            IronCrewError::Validation("PostgreSQL idempotency ttl_seconds is negative".into())
        })?;
    let state = row
        .try_get::<String, _>("state")
        .map_err(column_error)?
        .parse::<IdempotencyState>()?;
    let lease_expires_at: String = row.try_get("lease_expires_at").map_err(column_error)?;
    let lease_expires_at = if lease_expires_at.is_empty() {
        lease_expires_at
    } else {
        canonical_timestamp("stored idempotency lease expiry", &lease_expires_at)?
    };
    let created_at = canonical_timestamp(
        "stored idempotency creation time",
        &row.try_get::<String, _>("created_at")
            .map_err(column_error)?,
    )?;
    let updated_at = canonical_timestamp(
        "stored idempotency update time",
        &row.try_get::<String, _>("updated_at")
            .map_err(column_error)?,
    )?;
    let completed_at = row
        .try_get::<Option<String>, _>("completed_at")
        .map_err(column_error)?
        .map(|value| canonical_timestamp("stored idempotency completion time", &value))
        .transpose()?;
    let expires_at = row
        .try_get::<Option<String>, _>("expires_at")
        .map_err(column_error)?
        .map(|value| canonical_timestamp("stored idempotency retention expiry", &value))
        .transpose()?;

    let record = IdempotencyRecord {
        key_hash: row.try_get("key_hash").map_err(column_error)?,
        principal_id: PrincipalId::from_digest(row.try_get("principal_id").map_err(column_error)?)?,
        request_fingerprint: row.try_get("request_fingerprint").map_err(column_error)?,
        operation: row.try_get("operation").map_err(column_error)?,
        scope: row.try_get("scope").map_err(column_error)?,
        resource_id: row.try_get("resource_id").map_err(column_error)?,
        exclusive_scope: row.try_get("exclusive_scope").map_err(column_error)?,
        attempt_id: row.try_get("attempt_id").map_err(column_error)?,
        owner_instance_id: row.try_get("owner_instance_id").map_err(column_error)?,
        base_revision,
        state,
        response_status,
        response_body: row.try_get("response_body").map_err(column_error)?,
        lease_expires_at,
        created_at,
        updated_at,
        completed_at,
        expires_at,
        ttl_seconds,
    };
    record.validate()?;
    Ok(record)
}

fn nonnegative_accounting_value(label: &str, value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| {
        IronCrewError::Validation(format!(
            "PostgreSQL idempotency accounting value '{label}' is negative or out of range"
        ))
    })
}

fn column_error(error: sqlx::Error) -> IronCrewError {
    IronCrewError::Validation(format!("Column error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::decode_idempotency_accounting;

    #[test]
    fn accounting_decode_rejects_negative_database_values() {
        assert!(decode_idempotency_accounting((0, 0, 0)).is_ok());
        assert!(decode_idempotency_accounting((-1, 0, 0)).is_err());
        assert!(decode_idempotency_accounting((0, -1, 0)).is_err());
        assert!(decode_idempotency_accounting((0, 0, -1)).is_err());
    }
}

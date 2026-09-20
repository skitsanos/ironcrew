use crate::engine::idempotency::HARD_IDEMPOTENCY_RESPONSE_BYTES;

pub(in crate::engine::postgres_store) const IDEMPOTENCY_COLUMNS: &str = "key_hash, principal_id, request_fingerprint, operation, scope, \
     resource_id, exclusive_scope, attempt_id, owner_instance_id, base_revision, state, \
     response_status, response_body, lease_expires_at, created_at, updated_at, completed_at, \
     expires_at, ttl_seconds";

pub(in crate::engine::postgres_store) fn idempotency_select_columns() -> String {
    format!(
        "key_hash, principal_id, request_fingerprint, operation, scope, resource_id, \
         exclusive_scope, attempt_id, owner_instance_id, base_revision, state, response_status, \
         CASE WHEN response_body IS NULL OR octet_length(response_body) <= {limit} \
              THEN response_body END AS response_body, \
         COALESCE(octet_length(response_body), 0)::BIGINT AS response_body_bytes, \
         lease_expires_at, created_at, updated_at, completed_at, expires_at, ttl_seconds",
        limit = HARD_IDEMPOTENCY_RESPONSE_BYTES,
    )
}

#[derive(Debug, Clone, Copy)]
pub(in crate::engine::postgres_store) struct IdempotencyAccounting {
    pub(in crate::engine::postgres_store) records: usize,
    pub(in crate::engine::postgres_store) in_flight: usize,
    pub(in crate::engine::postgres_store) response_bytes: usize,
}

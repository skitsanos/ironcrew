use super::super::*;

impl PostgresStore {
    pub(super) async fn verify_idempotency_schema(&self) -> Result<()> {
        let idempotency_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('key_hash', 'text'), \
                 ('principal_id', 'text'), \
                 ('request_fingerprint', 'text'), \
                 ('operation', 'text'), \
                 ('scope', 'text'), \
                 ('resource_id', 'text'), \
                 ('exclusive_scope', 'text'), \
                 ('attempt_id', 'text'), \
                 ('owner_instance_id', 'text'), \
                 ('base_revision', 'bigint'), \
                 ('state', 'text'), \
                 ('response_status', 'integer'), \
                 ('response_body', 'text'), \
                 ('lease_expires_at', 'text'), \
                 ('created_at', 'text'), \
                 ('updated_at', 'text'), \
                 ('completed_at', 'text'), \
                 ('expires_at', 'text'), \
                 ('cancel_requested_at', 'text'), \
                 ('owner_draining_at', 'text'), \
                 ('ttl_seconds', 'bigint') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.idempotency_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency columns: {e}"
            ))
        })?;
        if idempotency_columns != 21 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more required typed columns",
                self.idempotency_table
            )));
        }

        let idempotency_primary_key: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 \
                 FROM pg_constraint con \
                 JOIN pg_class tbl ON tbl.oid = con.conrelid \
                 JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
                 JOIN pg_attribute attr \
                   ON attr.attrelid = tbl.oid AND attr.attnum = con.conkey[1] \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND con.contype = 'p' AND cardinality(con.conkey) = 1 \
                   AND attr.attname = 'key_hash'\
             )",
        )
        .bind(&self.idempotency_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency primary key: {e}"
            ))
        })?;
        if !idempotency_primary_key {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' must have key_hash as its primary key",
                self.idempotency_table
            )));
        }

        let expires_index = format!("{}_exp_idx", self.idempotency_table);
        let resource_index = format!("{}_res_idx", self.idempotency_table);
        let lease_index = format!("{}_lease_idx", self.idempotency_table);
        let scope_index = format!("{}_scope_uidx", self.idempotency_table);
        let owner_index = format!("{}_owner_idx", self.idempotency_table);
        let valid_idempotency_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM pg_index i \
             JOIN pg_class idx ON idx.oid = i.indexrelid \
             JOIN pg_class tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
               AND (\
                 (idx.relname = $2 AND i.indnkeyatts = 1 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at') \
                 OR \
                 (idx.relname = $3 AND i.indnkeyatts = 3 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'operation' \
                   AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'scope' \
                   AND pg_get_indexdef(i.indexrelid, 3, TRUE) = 'resource_id') \
                 OR \
                 (idx.relname = $4 AND i.indnkeyatts = 3 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'operation' \
                   AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'lease_expires_at' \
                   AND pg_get_indexdef(i.indexrelid, 3, TRUE) = 'key_hash' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%') \
                 OR \
                 (idx.relname = $5 AND i.indisunique AND i.indnkeyatts = 1 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'exclusive_scope' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%exclusive_scope IS NOT NULL%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%') \
                 OR \
                 (idx.relname = $6 AND i.indnkeyatts = 2 \
                   AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'owner_instance_id' \
                   AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'operation' \
                   AND i.indpred IS NOT NULL \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%claimed%' \
                   AND pg_get_expr(i.indpred, i.indrelid) LIKE '%running%')\
               )",
        )
        .bind(&self.idempotency_table)
        .bind(&expires_index)
        .bind(&resource_index)
        .bind(&lease_index)
        .bind(&scope_index)
        .bind(&owner_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency indexes: {e}"
            ))
        })?;
        if valid_idempotency_indexes != 5 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing one or more required idempotency indexes",
                self.idempotency_table
            )));
        }
        Ok(())
    }
}

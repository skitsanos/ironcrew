use super::super::*;

impl PostgresStore {
    pub(super) async fn verify_human_input_schema(&self) -> Result<()> {
        let human_input_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('run_id', 'text'), \
                 ('question_id', 'text'), \
                 ('flow', 'text'), \
                 ('owner_instance_id', 'text'), \
                 ('key_hash', 'text'), \
                 ('attempt_id', 'text'), \
                 ('question_digest', 'text'), \
                 ('question_key_fingerprint', 'text'), \
                 ('question_nonce', 'bytea'), \
                 ('question_ciphertext', 'bytea'), \
                 ('answer_key_fingerprint', 'text'), \
                 ('answer_nonce', 'bytea'), \
                 ('answer_ciphertext', 'bytea'), \
                 ('state', 'text'), \
                 ('created_at', 'timestamp with time zone'), \
                 ('expires_at', 'timestamp with time zone'), \
                 ('answered_at', 'timestamp with time zone') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.human_inputs_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox columns: {error}"
            ))
        })?;
        if human_input_columns != 17 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more human-input mailbox columns",
                self.human_inputs_table
            )));
        }

        let human_input_constraints: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint AS con \
             JOIN pg_class AS tbl ON tbl.oid = con.conrelid \
             JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
             LEFT JOIN pg_class AS referenced ON referenced.oid = con.confrelid \
             LEFT JOIN pg_namespace AS referenced_ns ON referenced_ns.oid = referenced.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 AND (\
                 (con.contype = 'p' AND cardinality(con.conkey) = 2 AND \
                  (SELECT array_agg(attr.attname ORDER BY key.ordinality) \
                   FROM unnest(con.conkey) WITH ORDINALITY AS key(attnum, ordinality) \
                   JOIN pg_attribute AS attr ON attr.attrelid = tbl.oid \
                                             AND attr.attnum = key.attnum) \
                    = ARRAY['run_id', 'question_id']::name[]) OR \
                 (con.contype = 'f' AND con.confdeltype = 'c' AND \
                  cardinality(con.conkey) = 1 AND \
                  (SELECT attr.attname FROM pg_attribute AS attr \
                   WHERE attr.attrelid = tbl.oid AND attr.attnum = con.conkey[1]) = 'run_id' AND \
                  referenced_ns.nspname = current_schema() AND referenced.relname = $5 AND \
                  (SELECT attr.attname FROM pg_attribute AS attr \
                   WHERE attr.attrelid = referenced.oid AND attr.attnum = con.confkey[1]) = 'run_id') OR \
                 (con.contype = 'c' AND con.conname IN ($2, $3, $4))\
             )",
        )
        .bind(&self.human_inputs_table)
        .bind(format!("{}_state_ck", self.human_inputs_table))
        .bind(format!("{}_payload_ck", self.human_inputs_table))
        .bind(format!("{}_expiry_ck", self.human_inputs_table))
        .bind(&self.table_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox constraints: {error}"
            ))
        })?;
        if human_input_constraints != 5 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing a required primary key, cascading run foreign key, or state/payload/expiry check",
                self.human_inputs_table
            )));
        }

        let human_run_index = format!("{}_run_idx", self.human_inputs_table);
        let human_expiry_index = format!("{}_exp_idx", self.human_inputs_table);
        let human_pending_expiry_index = format!("{}_pex_idx", self.human_inputs_table);
        let human_input_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_index AS i \
             JOIN pg_class AS idx ON idx.oid = i.indexrelid \
             JOIN pg_class AS tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() AND tbl.relname = $1 AND (\
                 (idx.relname = $2 AND i.indnkeyatts = 2 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'run_id' AND \
                  pg_get_indexdef(i.indexrelid, 2, TRUE) = 'expires_at' AND \
                  i.indpred IS NOT NULL AND \
                  pg_get_expr(i.indpred, i.indrelid) LIKE '%pending%') OR \
                 (idx.relname = $3 AND i.indnkeyatts = 1 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at') OR \
                 (idx.relname = $4 AND i.indnkeyatts = 3 AND \
                  pg_get_indexdef(i.indexrelid, 1, TRUE) = 'expires_at' AND \
                  pg_get_indexdef(i.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(i.indexrelid, 3, TRUE) = 'question_id' AND \
                  i.indpred IS NOT NULL AND \
                  pg_get_expr(i.indpred, i.indrelid) LIKE '%pending%')\
             )",
        )
        .bind(&self.human_inputs_table)
        .bind(&human_run_index)
        .bind(&human_expiry_index)
        .bind(&human_pending_expiry_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL human-input mailbox indexes: {error}"
            ))
        })?;
        if human_input_indexes != 3 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL table '{}' is missing one or more required human-input mailbox indexes",
                self.human_inputs_table
            )));
        }
        Ok(())
    }
}

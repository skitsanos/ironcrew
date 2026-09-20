use super::super::*;

impl PostgresStore {
    pub(super) async fn verify_core_schema(&self) -> Result<()> {
        let required_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
               AND (column_name, data_type) IN (\
                   ('owner_instance_id', 'text'), \
                   ('lease_expires_at', 'text'), \
                   ('task_results', 'jsonb'), \
                   ('tags', 'jsonb')\
               )",
        )
        .bind(&self.table_name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!("Failed to verify PostgreSQL run schema: {e}"))
        })?;
        if required_columns != 4 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more required typed columns",
                self.table_name
            )));
        }

        let conversation_index = format!("uniq_{}_flow_id", self.conversations_table);
        let dialog_index = format!("uniq_{}_flow_id", self.dialogs_table);
        let valid_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) \
             FROM pg_index i \
             JOIN pg_class idx ON idx.oid = i.indexrelid \
             JOIN pg_class tbl ON tbl.oid = i.indrelid \
             JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
             WHERE ns.nspname = current_schema() \
               AND ((tbl.relname = $1 AND idx.relname = $2) \
                 OR (tbl.relname = $3 AND idx.relname = $4)) \
               AND i.indisunique AND i.indnullsnotdistinct \
               AND i.indnkeyatts = 2 \
               AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'flow_path' \
               AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'id'",
        )
        .bind(&self.conversations_table)
        .bind(&conversation_index)
        .bind(&self.dialogs_table)
        .bind(&dialog_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!("Failed to verify PostgreSQL session schema: {e}"))
        })?;
        if valid_indexes != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL schema is missing a required UNIQUE NULLS NOT DISTINCT (flow_path, id) session index"
                    .into(),
            ));
        }
        let revision_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND ((table_name = $1 AND column_name = 'revision' AND data_type = 'bigint') \
                 OR (table_name = $2 AND column_name = 'revision' AND data_type = 'bigint'))",
        )
        .bind(&self.conversations_table)
        .bind(&self.dialogs_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL session revisions: {e}"
            ))
        })?;
        if revision_columns != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL session tables are missing required BIGINT revision columns".into(),
            ));
        }

        let conversation_execution_column: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
               AND column_name = 'execution' AND data_type = 'jsonb' \
               AND is_nullable = 'NO'",
        )
        .bind(&self.conversations_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL conversation execution identity: {error}"
            ))
        })?;
        if conversation_execution_column != 1 {
            return Err(IronCrewError::Validation(
                "PostgreSQL conversation table is missing the required non-null JSONB execution identity column"
                    .into(),
            ));
        }
        Ok(())
    }
}

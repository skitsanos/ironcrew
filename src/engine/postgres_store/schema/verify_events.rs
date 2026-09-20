use super::super::*;

impl PostgresStore {
    pub(super) async fn verify_run_event_schema(&self) -> Result<()> {
        let run_event_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS column_info \
             JOIN (VALUES \
                 ($1::text, 'run_id', 'text'), \
                 ($1::text, 'sequence', 'bigint'), \
                 ($1::text, 'event_type', 'text'), \
                 ($1::text, 'payload', 'jsonb'), \
                 ($1::text, 'payload_bytes', 'bigint'), \
                 ($1::text, 'accounted_bytes', 'bigint'), \
                 ($1::text, 'created_at', 'timestamp with time zone'), \
                 ($1::text, 'expires_at', 'timestamp with time zone'), \
                 ($2::text, 'run_id', 'text'), \
                 ($2::text, 'flow', 'text'), \
                 ($2::text, 'owner_instance_id', 'text'), \
                 ($2::text, 'latest_sequence', 'bigint'), \
                 ($2::text, 'dropped_through', 'bigint'), \
                 ($2::text, 'retained_events', 'bigint'), \
                 ($2::text, 'retained_bytes', 'bigint'), \
                 ($2::text, 'journal_complete', 'boolean'), \
                 ($2::text, 'eviction_reason', 'text'), \
                 ($2::text, 'terminal_event_sequence', 'bigint'), \
                 ($2::text, 'updated_at', 'timestamp with time zone'), \
                 ($3::text, 'singleton', 'boolean'), \
                 ($3::text, 'schema_version', 'integer'), \
                 ($3::text, 'retained_events', 'bigint'), \
                 ($3::text, 'retained_bytes', 'bigint'), \
                 ($3::text, 'updated_at', 'timestamp with time zone') \
             ) AS required(table_name, column_name, data_type) \
               ON required.table_name = column_info.table_name \
              AND required.column_name = column_info.column_name \
              AND required.data_type = column_info.data_type \
             WHERE column_info.table_schema = current_schema()",
        )
        .bind(&self.run_events_table)
        .bind(&self.run_event_state_table)
        .bind(&self.run_event_usage_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal columns: {error}"
            ))
        })?;
        if run_event_columns != 24 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing one or more required typed columns".into(),
            ));
        }

        let run_event_constraints: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint AS con \
             JOIN pg_class AS table_info ON table_info.oid = con.conrelid \
             JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
             LEFT JOIN pg_class AS referenced ON referenced.oid = con.confrelid \
             LEFT JOIN pg_namespace AS referenced_namespace \
                    ON referenced_namespace.oid = referenced.relnamespace \
             WHERE namespace.nspname = current_schema() AND (\
                 (table_info.relname = $1 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 2) OR \
                 (table_info.relname = $2 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 1) OR \
                 (table_info.relname = $3 AND con.contype = 'p' AND \
                  cardinality(con.conkey) = 1) OR \
                 (table_info.relname IN ($1, $2) AND con.contype = 'f' AND \
                  con.confdeltype = 'c' AND referenced_namespace.nspname = current_schema() AND \
                  referenced.relname = $4) OR \
                 (con.contype = 'c' AND con.conname IN ($5, $6, $7, $8, $9))\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&self.run_event_state_table)
        .bind(&self.run_event_usage_table)
        .bind(&self.table_name)
        .bind(format!("{}_payload_ck", self.run_events_table))
        .bind(format!("{}_expiry_ck", self.run_events_table))
        .bind(format!("{}_bounds_ck", self.run_event_state_table))
        .bind(format!("{}_reason_ck", self.run_event_state_table))
        .bind(format!("{}_usage_ck", self.run_event_usage_table))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal constraints: {error}"
            ))
        })?;
        if run_event_constraints != 10 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing a primary key, cascading run foreign key, or bounded-data constraint"
                    .into(),
            ));
        }

        let run_event_expiry_index = format!("{}_exp_idx", self.run_events_table);
        let run_event_oldest_index = format!("{}_old_idx", self.run_events_table);
        let run_event_indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_index AS index_info \
             JOIN pg_class AS index_class ON index_class.oid = index_info.indexrelid \
             JOIN pg_class AS table_info ON table_info.oid = index_info.indrelid \
             JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
             WHERE namespace.nspname = current_schema() AND table_info.relname = $1 AND (\
                 (index_class.relname = $2 AND index_info.indnkeyatts = 3 AND \
                  pg_get_indexdef(index_info.indexrelid, 1, TRUE) = 'expires_at' AND \
                  pg_get_indexdef(index_info.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(index_info.indexrelid, 3, TRUE) = 'sequence') OR \
                 (index_class.relname = $3 AND index_info.indnkeyatts = 3 AND \
                  pg_get_indexdef(index_info.indexrelid, 1, TRUE) = 'created_at' AND \
                  pg_get_indexdef(index_info.indexrelid, 2, TRUE) = 'run_id' AND \
                  pg_get_indexdef(index_info.indexrelid, 3, TRUE) = 'sequence')\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&run_event_expiry_index)
        .bind(&run_event_oldest_index)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event journal indexes: {error}"
            ))
        })?;
        if run_event_indexes != 2 {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event journal is missing a retention or global-pruning index"
                    .into(),
            ));
        }

        let run_event_trigger = format!("{}_acct_trg", self.run_events_table);
        let run_event_trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_trigger AS trigger_info \
                 JOIN pg_class AS table_info ON table_info.oid = trigger_info.tgrelid \
                 JOIN pg_namespace AS namespace ON namespace.oid = table_info.relnamespace \
                 WHERE namespace.nspname = current_schema() AND table_info.relname = $1 \
                   AND trigger_info.tgname = $2 AND NOT trigger_info.tgisinternal \
                   AND trigger_info.tgenabled <> 'D'\
             )",
        )
        .bind(&self.run_events_table)
        .bind(&run_event_trigger)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event accounting trigger: {error}"
            ))
        })?;
        if !run_event_trigger_exists {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event accounting trigger is missing or disabled".into(),
            ));
        }

        let run_event_usage_valid: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {} WHERE singleton = TRUE \
                     AND schema_version = {RUN_EVENT_SCHEMA_VERSION} \
                     AND retained_events >= 0 AND retained_bytes >= 0)",
            self.run_event_usage_table
        )))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL run-event global accounting: {error}"
            ))
        })?;
        if !run_event_usage_valid {
            return Err(IronCrewError::Validation(
                "PostgreSQL run-event global accounting row is missing or invalid".into(),
            ));
        }
        Ok(())
    }
}

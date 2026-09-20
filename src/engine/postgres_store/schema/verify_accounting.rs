use super::super::*;

impl PostgresStore {
    pub(super) async fn verify_idempotency_accounting_schema(&self) -> Result<()> {
        let accounting_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns AS c \
             JOIN (VALUES \
                 ('principal_id', 'text'), \
                 ('is_global', 'boolean'), \
                 ('record_count', 'bigint'), \
                 ('in_flight_count', 'bigint'), \
                 ('response_bytes', 'bigint'), \
                 ('updated_at', 'timestamp with time zone') \
             ) AS required(column_name, data_type) \
               ON required.column_name = c.column_name \
              AND required.data_type = c.data_type \
             WHERE c.table_schema = current_schema() AND c.table_name = $1",
        )
        .bind(&self.idempotency_accounting_table)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency accounting columns: {error}"
            ))
        })?;
        if accounting_columns != 6 {
            return Err(IronCrewError::Validation(format!(
                "PostgreSQL schema for '{}' is missing one or more accounting columns",
                self.idempotency_accounting_table
            )));
        }
        let accounting_trigger = format!("{}_acct_trg", self.idempotency_table);
        let trigger_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_trigger AS trg \
                 JOIN pg_class AS tbl ON tbl.oid = trg.tgrelid \
                 JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND trg.tgname = $2 AND NOT trg.tgisinternal \
                   AND trg.tgenabled <> 'D'\
             )",
        )
        .bind(&self.idempotency_table)
        .bind(&accounting_trigger)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL idempotency accounting trigger: {error}"
            ))
        })?;
        if !trigger_exists {
            return Err(IronCrewError::Validation(
                "PostgreSQL idempotency accounting trigger is missing or disabled".into(),
            ));
        }
        let global_accounting_valid: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {} \
                 WHERE principal_id = 'global' AND is_global = TRUE \
                   AND record_count >= 0 AND in_flight_count >= 0 AND response_bytes >= 0)",
            self.idempotency_accounting_table
        )))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to verify PostgreSQL global idempotency accounting: {error}"
            ))
        })?;
        if !global_accounting_valid {
            return Err(IronCrewError::Validation(
                "PostgreSQL global idempotency accounting row is missing or invalid".into(),
            ));
        }
        Ok(())
    }
}

use super::super::*;

impl PostgresStore {
    pub(super) async fn verify_bootstrap_run_columns(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        let t = &self.table_name;
        for (column, data_type) in [
            ("owner_instance_id", "text"),
            ("lease_expires_at", "text"),
            ("task_results", "jsonb"),
            ("tags", "jsonb"),
        ] {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS (\
                    SELECT 1 FROM information_schema.columns \
                    WHERE table_schema = current_schema() \
                      AND table_name = $1 AND column_name = $2 AND data_type = $3\
                )",
            )
            .bind(t)
            .bind(column)
            .bind(data_type)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to verify required run column '{column}': {e}"
                ))
            })?;
            if !valid {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL column '{t}.{column}' is missing or is not {data_type}"
                )));
            }
        }
        Ok(())
    }
}

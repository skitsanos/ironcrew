use super::super::*;

impl PostgresStore {
    pub(super) async fn bootstrap_audit(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        // 6. Audit events table
        let at = &self.audit_events_table;
        let audit_sql = format!(
            "CREATE TABLE IF NOT EXISTS {at} (
                id          TEXT PRIMARY KEY,
                timestamp   TEXT NOT NULL,
                action      TEXT NOT NULL,
                flow_path   TEXT,
                target      TEXT,
                actor       TEXT,
                source_ip   TEXT,
                success     BOOLEAN NOT NULL,
                status_code INTEGER NOT NULL,
                metadata    JSONB
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(audit_sql.to_string()))
            .execute(&mut **tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("Failed to create {at} table: {e}")))?;

        let audit_indexes: &[String] = &[
            format!("CREATE INDEX IF NOT EXISTS idx_{at}_timestamp_desc ON {at} (timestamp DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{at}_flow_path ON {at} (flow_path)"),
        ];
        for sql in audit_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create audit index: {e}"))
                })?;
        }
        Ok(())
    }
}

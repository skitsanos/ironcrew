use super::super::*;

impl PostgresStore {
    pub(super) async fn bootstrap_sessions(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        // 5. Session tables — conversations and dialogs for resumable sessions
        let ct = &self.conversations_table;
        let dt = &self.dialogs_table;

        let session_tables = [
            format!(
                "CREATE TABLE IF NOT EXISTS {ct} (
                    id          TEXT PRIMARY KEY,
                    flow_name   TEXT NOT NULL,
                    agent_name  TEXT NOT NULL,
                    execution   JSONB NOT NULL DEFAULT '{{}}',
                    messages    JSONB NOT NULL DEFAULT '[]',
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    revision    BIGINT NOT NULL DEFAULT 0
                )"
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {dt} (
                    id          TEXT PRIMARY KEY,
                    flow_name   TEXT NOT NULL,
                    agent_names JSONB NOT NULL DEFAULT '[]',
                    starter     TEXT NOT NULL,
                    transcript  JSONB NOT NULL DEFAULT '[]',
                    next_index  INTEGER NOT NULL,
                    stopped     BOOLEAN NOT NULL DEFAULT FALSE,
                    stop_reason TEXT,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    revision    BIGINT NOT NULL DEFAULT 0
                )"
            ),
        ];
        for sql in &session_tables {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create session table: {}", e))
                })?;
        }

        // Add flow_path column for schemas predating Phase-1 HITL support.
        // Guarded with IF NOT EXISTS for idempotency, matching the run-history
        // migrations in the same schema transaction.
        let session_migrations: &[(&str, String)] = &[
            (
                "conversations.flow_path",
                format!("ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS flow_path TEXT"),
            ),
            (
                "dialogs.flow_path",
                format!("ALTER TABLE {dt} ADD COLUMN IF NOT EXISTS flow_path TEXT"),
            ),
            (
                "conversations.revision",
                format!(
                    "ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS revision BIGINT NOT NULL DEFAULT 0"
                ),
            ),
            (
                "conversations.execution",
                format!(
                    "ALTER TABLE {ct} ADD COLUMN IF NOT EXISTS execution JSONB NOT NULL DEFAULT '{{}}'"
                ),
            ),
            (
                "dialogs.revision",
                format!(
                    "ALTER TABLE {dt} ADD COLUMN IF NOT EXISTS revision BIGINT NOT NULL DEFAULT 0"
                ),
            ),
        ];
        for (label, sql) in session_migrations {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate session column '{label}': {e}"
                    ))
                })?;
        }

        // Enforce the documented `(flow_path, id)` uniqueness for sessions.
        // Earlier versions used `id` as the sole PRIMARY KEY, which meant a
        // save from flow-B would overwrite flow-A's session with the same
        // id. PostgreSQL 15+ is required so we can use `NULLS NOT DISTINCT`
        // and preserve deterministic uniqueness for legacy `flow_path IS NULL`
        // rows as well.
        let session_unique_indexes: &[(&str, String)] = &[
            (
                "conversations: composite unique (flow_path, id)",
                format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS uniq_{ct}_flow_id \
                     ON {ct} (flow_path, id) NULLS NOT DISTINCT"
                ),
            ),
            (
                "dialogs: composite unique (flow_path, id)",
                format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS uniq_{dt}_flow_id \
                     ON {dt} (flow_path, id) NULLS NOT DISTINCT"
                ),
            ),
        ];
        for (label, sql) in session_unique_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to enforce session uniqueness '{label}': {e}"
                    ))
                })?;
        }

        // `CREATE INDEX IF NOT EXISTS` accepts a same-named but incompatible
        // index. Verify the exact safety properties before dropping the old
        // primary keys; a mismatch rolls the entire transaction back.
        for (table, index) in [
            (ct.as_str(), format!("uniq_{ct}_flow_id")),
            (dt.as_str(), format!("uniq_{dt}_flow_id")),
        ] {
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS (\
                    SELECT 1 \
                    FROM pg_index i \
                    JOIN pg_class idx ON idx.oid = i.indexrelid \
                    JOIN pg_class tbl ON tbl.oid = i.indrelid \
                    JOIN pg_namespace ns ON ns.oid = tbl.relnamespace \
                    WHERE ns.nspname = current_schema() \
                      AND tbl.relname = $1 AND idx.relname = $2 \
                      AND i.indisunique AND i.indnullsnotdistinct \
                      AND i.indnkeyatts = 2 \
                      AND pg_get_indexdef(i.indexrelid, 1, TRUE) = 'flow_path' \
                      AND pg_get_indexdef(i.indexrelid, 2, TRUE) = 'id'\
                )",
            )
            .bind(table)
            .bind(&index)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to verify new PostgreSQL session index '{index}': {e}"
                ))
            })?;
            if !valid {
                return Err(IronCrewError::Validation(format!(
                    "PostgreSQL index '{index}' exists without the required UNIQUE NULLS NOT DISTINCT (flow_path, id) properties"
                )));
            }
        }

        for (label, sql) in [
            (
                "conversations: drop legacy id PK",
                format!("ALTER TABLE {ct} DROP CONSTRAINT IF EXISTS {ct}_pkey"),
            ),
            (
                "dialogs: drop legacy id PK",
                format!("ALTER TABLE {dt} DROP CONSTRAINT IF EXISTS {dt}_pkey"),
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to remove legacy session constraint '{label}': {e}"
                    ))
                })?;
        }

        // Session indexes — updated_at helps "list recent sessions" queries
        let session_indexes = [
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_updated_at ON {ct} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_flow_name ON {ct} (flow_name)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{ct}_flow_path ON {ct} (flow_path)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_updated_at ON {dt} (updated_at DESC)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_flow_name ON {dt} (flow_name)"),
            format!("CREATE INDEX IF NOT EXISTS idx_{dt}_flow_path ON {dt} (flow_path)"),
        ];
        for sql in &session_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create session index: {e}"))
                })?;
        }
        Ok(())
    }
}

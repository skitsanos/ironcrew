use super::super::*;

impl PostgresStore {
    pub(super) async fn bootstrap_runs(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        let t = &self.table_name;
        let unknown_usage = serde_json::to_string(&crate::usage::UsageSnapshot::unavailable())
            .expect("scalar usage snapshot serializes");
        // 1. Create table if not exists
        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS {t} (
                run_id        TEXT PRIMARY KEY,
                flow_name     TEXT NOT NULL,
                flow          TEXT NOT NULL DEFAULT '',
                status        TEXT NOT NULL,
                started_at    TEXT NOT NULL,
                finished_at   TEXT NOT NULL,
                duration_ms   BIGINT NOT NULL,
                task_results  JSONB NOT NULL DEFAULT '[]',
                agent_count   INTEGER NOT NULL,
                task_count    INTEGER NOT NULL,
                usage JSONB NOT NULL DEFAULT '{unknown_usage}',
                tags          JSONB DEFAULT '[]',
                owner_instance_id TEXT NOT NULL DEFAULT '',
                lease_expires_at TEXT NOT NULL DEFAULT '',
                created_at    TIMESTAMPTZ DEFAULT NOW()
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(create_sql.to_string()))
            .execute(&mut **tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("Failed to create {t} table: {e}")))?;

        // 2. Add missing columns (heal older schema versions)
        let migrations: &[(&str, &str)] = &[
            (
                "flow",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS flow TEXT NOT NULL DEFAULT ''"),
            ),
            (
                "usage",
                &format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS usage JSONB NOT NULL DEFAULT '{unknown_usage}'"
                ),
            ),
            (
                "tags",
                &format!("ALTER TABLE {t} ADD COLUMN IF NOT EXISTS tags JSONB DEFAULT '[]'"),
            ),
            (
                "created_at",
                &format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ DEFAULT NOW()"
                ),
            ),
        ];

        for (col, sql) in migrations {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate required run column '{col}': {e}"
                    ))
                })?;
        }

        // Ownership columns are safety-critical: continuing without them
        // would restore global startup abandonment semantics. Fail startup if
        // the database role cannot apply this backward-compatible migration.
        for (column, sql) in [
            (
                "owner_instance_id",
                format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS owner_instance_id TEXT NOT NULL DEFAULT ''"
                ),
            ),
            (
                "lease_expires_at",
                format!(
                    "ALTER TABLE {t} ADD COLUMN IF NOT EXISTS lease_expires_at TEXT NOT NULL DEFAULT ''"
                ),
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate required run ownership column '{}': {}",
                        column, e
                    ))
                })?;
        }

        // 3. Heal column types — upgrade TEXT to JSONB if needed
        let type_fixes: &[(&str, &str)] = &[
            ("task_results", &format!(
                "DO $$ BEGIN
                    IF EXISTS (
                        SELECT 1 FROM information_schema.columns
                        WHERE table_name = '{t}' AND column_name = 'task_results' AND data_type = 'text'
                    ) THEN
                        ALTER TABLE {t} ALTER COLUMN task_results TYPE JSONB USING task_results::jsonb;
                        RAISE NOTICE 'Upgraded task_results from TEXT to JSONB';
                    END IF;
                END $$"
            )),
            ("tags", &format!(
                "DO $$ BEGIN
                    IF EXISTS (
                        SELECT 1 FROM information_schema.columns
                        WHERE table_name = '{t}' AND column_name = 'tags' AND data_type = 'text'
                    ) THEN
                        ALTER TABLE {t} ALTER COLUMN tags TYPE JSONB USING tags::jsonb;
                        RAISE NOTICE 'Upgraded tags from TEXT to JSONB';
                    END IF;
                END $$"
            )),
        ];

        for (col, sql) in type_fixes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to migrate run column type '{col}': {e}"
                    ))
                })?;
        }

        // 4. Create indexes (IF NOT EXISTS — safe to run repeatedly)
        let indexes: &[&str] = &[
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_status ON {t} (status)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_started_at ON {t} (started_at DESC)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_flow_name ON {t} (flow_name)"),
            &format!("CREATE INDEX IF NOT EXISTS idx_{t}_tags ON {t} USING GIN (tags)"),
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_{t}_task_results ON {t} USING GIN (task_results)"
            ),
            &format!(
                "CREATE INDEX IF NOT EXISTS idx_{t}_active_lease ON {t} (status, lease_expires_at)"
            ),
        ];

        for sql in indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("Failed to create run index: {e}"))
                })?;
        }
        Ok(())
    }
}

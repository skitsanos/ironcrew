use super::super::*;

impl PostgresStore {
    pub(super) async fn bootstrap_idempotency(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        // 7. Durable request idempotency. Keep identifiers derived solely
        // from the validated table prefix; the compact suffixes also keep
        // every index name below PostgreSQL's 63-byte identifier limit at
        // the maximum supported prefix length.
        let it = &self.idempotency_table;
        let idempotency_sql = format!(
            "CREATE TABLE IF NOT EXISTS {it} (
                key_hash            TEXT PRIMARY KEY,
                principal_id        TEXT NOT NULL,
                request_fingerprint TEXT NOT NULL,
                operation           TEXT NOT NULL,
                scope               TEXT NOT NULL,
                resource_id         TEXT NOT NULL,
                exclusive_scope     TEXT,
                attempt_id          TEXT NOT NULL,
                owner_instance_id   TEXT NOT NULL,
                base_revision       BIGINT,
                state               TEXT NOT NULL,
                response_status     INTEGER,
                response_body       TEXT,
                lease_expires_at    TEXT NOT NULL,
                created_at          TEXT NOT NULL,
                updated_at          TEXT NOT NULL,
                completed_at        TEXT,
                expires_at          TEXT,
                cancel_requested_at TEXT,
                owner_draining_at   TEXT,
                ttl_seconds         BIGINT NOT NULL,
                CHECK (state IN ('claimed', 'running', 'completed', 'indeterminate')),
                CHECK (response_status IS NULL OR response_status BETWEEN 100 AND 599),
                CHECK (ttl_seconds > 0)
            )"
        );
        sqlx::query(sqlx::AssertSqlSafe(idempotency_sql))
            .execute(&mut **tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "Failed to create PostgreSQL idempotency table '{it}': {e}"
                ))
            })?;

        // Cross-replica run cancellation uses the keyed run ledger as a
        // durable mailbox. This nullable timestamp is intentionally separate
        // from the replay response so existing clients and ledgers remain
        // backwards compatible.
        let add_cancel_requested =
            format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS cancel_requested_at TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_cancel_requested))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotent-run cancellation column: {error}"
                ))
            })?;

        // Process drain is fenced on each exact in-flight run ledger. A
        // nullable timestamp preserves old rows while preventing a coarse
        // instance marker from poisoning a later process that reuses a name.
        let add_owner_draining =
            format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS owner_draining_at TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_owner_draining))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotent-run owner-drain column: {error}"
                ))
            })?;

        // Backfill ledgers created before principal-aware admission. The
        // opaque legacy digest preserves their non-reusability across an
        // upgrade without persisting a bearer credential or raw label.
        let add_principal = format!("ALTER TABLE {it} ADD COLUMN IF NOT EXISTS principal_id TEXT");
        sqlx::query(sqlx::AssertSqlSafe(add_principal))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to add PostgreSQL idempotency principal column: {error}"
                ))
            })?;
        let backfill_principal = format!(
            "UPDATE {it} SET principal_id = $1 \
             WHERE principal_id IS NULL OR principal_id = ''"
        );
        sqlx::query(sqlx::AssertSqlSafe(backfill_principal))
            .bind(PrincipalId::legacy().as_str())
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to migrate PostgreSQL idempotency principals: {error}"
                ))
            })?;
        let require_principal = format!("ALTER TABLE {it} ALTER COLUMN principal_id SET NOT NULL");
        sqlx::query(sqlx::AssertSqlSafe(require_principal))
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to require PostgreSQL idempotency principals: {error}"
                ))
            })?;
        let principal_constraint = format!("{it}_principal_ck");
        let has_principal_constraint: bool = sqlx::query_scalar(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_constraint AS con \
                 JOIN pg_class AS tbl ON tbl.oid = con.conrelid \
                 JOIN pg_namespace AS ns ON ns.oid = tbl.relnamespace \
                 WHERE ns.nspname = current_schema() AND tbl.relname = $1 \
                   AND con.conname = $2 AND con.contype = 'c'\
             )",
        )
        .bind(it)
        .bind(&principal_constraint)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to inspect PostgreSQL idempotency principal constraint: {error}"
            ))
        })?;
        if !has_principal_constraint {
            let sql = format!(
                "ALTER TABLE {it} ADD CONSTRAINT {principal_constraint} \
                 CHECK (length(principal_id) = 64 AND principal_id !~ '[^0-9a-f]')"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut **tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "Failed to constrain PostgreSQL idempotency principals: {error}"
                    ))
                })?;
        }

        let idempotency_indexes = [
            format!("CREATE INDEX IF NOT EXISTS {it}_exp_idx ON {it} (expires_at)"),
            format!(
                "CREATE INDEX IF NOT EXISTS {it}_res_idx \
                 ON {it} (operation, scope, resource_id)"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {it}_lease_idx \
                 ON {it} (operation, lease_expires_at, key_hash) \
                 WHERE state IN ('claimed', 'running')"
            ),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {it}_scope_uidx \
                 ON {it} (exclusive_scope) \
                 WHERE exclusive_scope IS NOT NULL \
                   AND state IN ('claimed', 'running')"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {it}_owner_idx \
                 ON {it} (owner_instance_id, operation) \
                 WHERE state IN ('claimed', 'running')"
            ),
        ];
        for sql in &idempotency_indexes {
            sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .execute(&mut **tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!(
                        "Failed to create PostgreSQL idempotency index: {e}"
                    ))
                })?;
        }

        Ok(())
    }
}

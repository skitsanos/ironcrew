use super::*;

impl PostgresStore {
    pub(super) async fn lock_advisory(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        domain: &str,
        identity: &str,
        shared: bool,
    ) -> Result<()> {
        let lock_name = format!(
            "ironcrew:{}:{domain}:{}:{identity}",
            self.idempotency_table,
            identity.len()
        );
        let sql = if shared {
            "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))"
        } else {
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))"
        };
        sqlx::query(sql)
            .bind(lock_name)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "Failed to acquire PostgreSQL {domain} advisory lock: {error}"
                ))
            })?;
        Ok(())
    }

    pub(super) async fn configure_run_lease_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<()> {
        // PostgreSQL applies these limits per statement, not to the aggregate
        // transaction. The outer maintenance watchdog may therefore fire
        // first after several individually successful statements. Dropping
        // SQLx's owned transaction schedules a rollback before that pooled
        // connection can be reused; the maintenance regression suite covers
        // both the atomic rollback and subsequent pool recovery.
        let timeout = crate::engine::store::run_maintenance_database_timeout(self.lease.ttl());
        let timeout_value = format!("{}ms", timeout.as_millis());
        sqlx::query(
            "SELECT set_config('lock_timeout', $1, true), \
                    set_config('statement_timeout', $1, true)",
        )
        .bind(timeout_value)
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to configure PostgreSQL run-lease transaction timeouts: {error}"
            ))
        })?;
        Ok(())
    }

    pub(super) async fn lock_resource(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        operation: &str,
        scope: &str,
        resource_id: &str,
    ) -> Result<()> {
        let identity = format!(
            "{}:{operation}:{}:{scope}:{}:{resource_id}",
            operation.len(),
            scope.len(),
            resource_id.len()
        );
        self.lock_advisory(tx, "resource", &identity, false).await
    }

    pub(super) async fn lock_run_fence(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        shared: bool,
    ) -> Result<()> {
        self.lock_advisory(tx, "run-fence", "global", shared).await
    }

    pub(super) async fn materialize_abandoned_claim_batch(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "WITH candidates AS (\
                 SELECT idem.resource_id, idem.scope, idem.created_at, \
                        idem.owner_instance_id \
                 FROM {idempotency} AS idem \
                 WHERE idem.operation = $2 AND idem.state = 'claimed' \
                   AND idem.lease_expires_at::timestamptz <= $3::timestamptz \
                   AND NOT EXISTS (\
                       SELECT 1 FROM {runs} AS run \
                       WHERE run.run_id = idem.resource_id\
                   ) \
                 ORDER BY idem.lease_expires_at, idem.key_hash \
                 LIMIT $4 FOR UPDATE OF idem SKIP LOCKED\
             ) \
             INSERT INTO {runs} (\
                 run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
                 task_results, agent_count, task_count, tags, \
                 owner_instance_id, lease_expires_at\
             ) \
             SELECT resource_id, scope, scope, 'abandoned', created_at, $1, 0, \
                    '[]'::jsonb, 0, 0, '[]'::jsonb, owner_instance_id, '' \
             FROM candidates \
             ON CONFLICT (run_id) DO NOTHING \
             RETURNING run_id",
            runs = self.table_name,
            idempotency = self.idempotency_table,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(RUN_OPERATION)
            .bind(database_now)
            .bind(limit)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PG idempotent run fallback: {error}"))
            })?;
        rows.into_iter()
            .map(|row| {
                row.try_get("run_id").map_err(|error| {
                    IronCrewError::Validation(format!("PostgreSQL fallback run id column: {error}"))
                })
            })
            .collect()
    }

    pub(super) async fn reconcile_expired_run_batch(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
        limit: i64,
    ) -> Result<Vec<String>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "WITH candidates AS (\
                 SELECT run_id FROM {runs} \
                 WHERE status IN ('running', 'waiting_for_input') \
                   AND (lease_expires_at = '' OR \
                        lease_expires_at::timestamptz <= $2::timestamptz) \
                 ORDER BY lease_expires_at, run_id \
                 LIMIT $3 FOR UPDATE SKIP LOCKED\
             ) \
             UPDATE {runs} AS run \
             SET status = 'abandoned', finished_at = $1, lease_expires_at = '' \
             FROM candidates \
             WHERE run.run_id = candidates.run_id \
             RETURNING run.run_id",
            runs = self.table_name,
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(database_now)
            .bind(database_now)
            .bind(limit)
            .fetch_all(&mut **tx)
            .await
            .map_err(|error| IronCrewError::Validation(format!("PG reconcile: {error}")))?;
        rows.into_iter()
            .map(|row| {
                row.try_get("run_id").map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PostgreSQL reconciled run id column: {error}"
                    ))
                })
            })
            .collect()
    }

    pub(super) async fn finalize_reconciled_runs(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        database_now: &str,
        run_ids: &[String],
    ) -> Result<()> {
        if run_ids.is_empty() {
            return Ok(());
        }
        let journal_sql = format!(
            "UPDATE {} SET journal_complete = FALSE, updated_at = clock_timestamp() \
             WHERE run_id = ANY($1::text[])",
            self.run_event_state_table,
        );
        sqlx::query(sqlx::AssertSqlSafe(journal_sql))
            .bind(run_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL abandoned run-event journal update failed: {error}"
                ))
            })?;

        let mapping_sql = format!(
            "UPDATE {idempotency} \
             SET state = 'completed', lease_expires_at = '', \
                 updated_at = $2, completed_at = $2, \
                 expires_at = to_char(\
                     ($2::timestamptz + ttl_seconds * interval '1 second') \
                         AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             WHERE operation = $1 AND resource_id = ANY($3::text[]) \
               AND state IN ('claimed', 'running', 'indeterminate')",
            idempotency = self.idempotency_table,
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(RUN_OPERATION)
            .bind(database_now)
            .bind(run_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG reconciled run idempotency transition: {error}"
                ))
            })?;

        let mailbox_sql = format!(
            "DELETE FROM {} WHERE run_id = ANY($1::text[])",
            self.human_inputs_table,
        );
        sqlx::query(sqlx::AssertSqlSafe(mailbox_sql))
            .bind(run_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PostgreSQL reconciled run mailbox cleanup failed: {error}"
                ))
            })?;
        Ok(())
    }

    pub(super) async fn database_clock_with_deadline(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        seconds: u64,
        context: &str,
    ) -> Result<(String, String)> {
        let seconds = i64::try_from(seconds).map_err(|_| {
            IronCrewError::Validation(format!("PostgreSQL {context} duration is out of range"))
        })?;
        sqlx::query_as::<_, (String, String)>(
            "WITH db_clock AS (SELECT clock_timestamp() AS now) \
             SELECT \
                 to_char(now AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'), \
                 to_char(\
                     (now + $1::bigint * interval '1 second') AT TIME ZONE 'UTC', \
                     'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
                 ) \
             FROM db_clock",
        )
        .bind(seconds)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| {
            IronCrewError::Validation(format!(
                "Failed to read PostgreSQL clock for {context}: {error}"
            ))
        })
    }
}

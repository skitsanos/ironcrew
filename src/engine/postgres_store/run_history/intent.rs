use crate::engine::idempotency::RUN_OPERATION;
use crate::engine::run_history::RunIntent;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn insert_run_intent(
        &self,
        intent: RunIntent,
    ) -> Result<String> {
        let run_id = intent
            .suggested_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let tags_json = serde_json::to_string(&intent.tags)
            .map_err(|e| IronCrewError::Validation(format!("Tags serialize: {}", e)))?;
        let empty_tasks = serde_json::to_string(&serde_json::Value::Array(Vec::new()))
            .map_err(|e| IronCrewError::Validation(format!("Empty tasks serialize: {}", e)))?;
        let sql = format!(
            "INSERT INTO {} (run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results, agent_count, task_count, total_tokens, cached_tokens, tags, owner_instance_id, lease_expires_at)
             VALUES ($1, $2, $3, 'running', $4, '', 0, $5::jsonb, $6, $7, 0, 0, $8::jsonb, $9, $10)
             ON CONFLICT (run_id) DO NOTHING",
            self.table_name
        );
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG insert intent transaction: {error}"))
        })?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", &run_id)
            .await?;
        let (database_now, lease_expires_at) = self
            .database_clock_with_deadline(&mut tx, self.lease.ttl().as_secs(), "run intent lease")
            .await?;
        let inserted = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&run_id)
            .bind(&intent.flow_name)
            .bind(&intent.flow)
            .bind(&intent.started_at)
            .bind(&empty_tasks)
            .bind(intent.agent_count as i64)
            .bind(intent.task_count as i64)
            .bind(&tags_json)
            .bind(self.lease.instance_id())
            .bind(&lease_expires_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG insert intent: {}", e)))?;
        let inserted_new = inserted.rows_affected() == 1;
        if !inserted_new {
            let hydrate_sql = format!(
                "UPDATE {runs} AS run SET \
                     flow_name = $1, agent_count = $2, task_count = $3, \
                     tags = $4::jsonb, lease_expires_at = $5 \
                 WHERE run.run_id = $6 AND run.flow = $7 \
                   AND run.owner_instance_id = $8 \
                   AND run.status IN ('running', 'waiting_for_input') \
                   AND EXISTS (\
                       SELECT 1 FROM {idempotency} AS idem \
                       WHERE idem.operation = $9 AND idem.scope = $7 \
                         AND idem.resource_id = $6 \
                         AND idem.owner_instance_id = $8 \
                         AND idem.state IN ('running', 'completed') \
                         AND idem.owner_draining_at IS NULL\
                   )",
                runs = self.table_name,
                idempotency = self.idempotency_table
            );
            let hydrated = sqlx::query(sqlx::AssertSqlSafe(hydrate_sql))
                .bind(&intent.flow_name)
                .bind(intent.agent_count as i64)
                .bind(intent.task_count as i64)
                .bind(&tags_json)
                .bind(&lease_expires_at)
                .bind(&run_id)
                .bind(&intent.flow)
                .bind(self.lease.instance_id())
                .bind(RUN_OPERATION)
                .execute(&mut *tx)
                .await
                .map_err(|error| {
                    IronCrewError::Validation(format!(
                        "PG idempotent provisional run hydration: {error}"
                    ))
                })?;
            if hydrated.rows_affected() != 1 {
                let drain_sql = format!(
                    "SELECT owner_draining_at FROM {} \
                     WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
                       AND owner_instance_id = $4",
                    self.idempotency_table
                );
                let draining: Option<Option<String>> =
                    sqlx::query_scalar(sqlx::AssertSqlSafe(drain_sql))
                        .bind(RUN_OPERATION)
                        .bind(&intent.flow)
                        .bind(&run_id)
                        .bind(self.lease.instance_id())
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(|error| {
                            IronCrewError::Validation(format!(
                                "PG provisional run owner-drain verification: {error}"
                            ))
                        })?;
                if draining.flatten().is_some() {
                    return Err(IronCrewError::OwnerDraining {
                        owner_instance_id: self.lease.instance_id().to_string(),
                    });
                }
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' already exists without a matching idempotent provisional intent"
                )));
            }
        }
        let mapping_sql = format!(
            "UPDATE {} SET state = 'running', lease_expires_at = $1, updated_at = $2 \
             WHERE operation = $3 AND scope = $4 AND resource_id = $5 \
               AND owner_instance_id = $6 AND state = 'claimed' \
               AND cancel_requested_at IS NULL \
               AND owner_draining_at IS NULL \
               AND lease_expires_at::timestamptz > $2::timestamptz",
            self.idempotency_table
        );
        let mapped = sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(&lease_expires_at)
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(&intent.flow)
            .bind(&run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!("PG run idempotency mapping transition: {error}"))
            })?;
        if mapped.rows_affected() == 0 {
            let linked_sql = format!(
                "SELECT owner_draining_at FROM {} \
                 WHERE operation = $1 AND scope = $2 AND resource_id = $3 \
                   AND owner_instance_id = $4",
                self.idempotency_table
            );
            let linked: Option<Option<String>> =
                sqlx::query_scalar(sqlx::AssertSqlSafe(linked_sql))
                    .bind(RUN_OPERATION)
                    .bind(&intent.flow)
                    .bind(&run_id)
                    .bind(self.lease.instance_id())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| {
                        IronCrewError::Validation(format!(
                            "PG run idempotency mapping verification: {error}"
                        ))
                    })?;
            if linked.as_ref().is_some_and(Option::is_some) {
                return Err(IronCrewError::OwnerDraining {
                    owner_instance_id: self.lease.instance_id().to_string(),
                });
            }
            if inserted_new && linked.is_some() {
                return Err(IronCrewError::Conflict(format!(
                    "Run '{run_id}' cannot start because its idempotency claim expired or was cancelled"
                )));
            }
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PG insert intent commit: {error}"))
        })?;
        tracing::debug!("Run intent saved: {}", run_id);
        Ok(run_id)
    }
}

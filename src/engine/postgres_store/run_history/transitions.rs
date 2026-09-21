use sqlx::Row;

use crate::engine::idempotency::RUN_OPERATION;
use crate::engine::run_history::{RunCompletion, RunStatus, RunTransition};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn finish_run(
        &self,
        run_id: &str,
        completion: RunCompletion,
    ) -> Result<RunTransition> {
        completion.validate()?;
        let task_results_json = serde_json::to_string(&completion.task_results)
            .map_err(|e| IronCrewError::Validation(format!("task_results serialize: {}", e)))?;
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PG update completion transaction: {error}"))
        })?;
        self.lock_idempotency_quota(&mut tx).await?;
        self.lock_run_fence(&mut tx, true).await?;
        self.lock_resource(&mut tx, RUN_OPERATION, "", run_id)
            .await?;
        let (database_now, _) = self
            .database_clock_with_deadline(&mut tx, 0, "run completion")
            .await?;
        let sql = format!(
            "UPDATE {}
             SET status = $1, finished_at = $2, duration_ms = $3,
                 task_results = $4::jsonb, usage = $5,
                 lease_expires_at = ''
             WHERE run_id = $6 AND status IN ('running', 'waiting_for_input')
               AND owner_instance_id = $7",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(completion.status.to_string())
            .bind(&completion.finished_at)
            .bind(completion.duration_ms as i64)
            .bind(&task_results_json)
            .bind(sqlx::types::Json(&completion.usage))
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG update completion: {}", e)))?;

        let transition = if result.rows_affected() == 0 {
            let sql = format!(
                "SELECT status, owner_instance_id, finished_at FROM {} WHERE run_id = $1 FOR UPDATE",
                self.table_name
            );
            let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
                .bind(run_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| {
                    IronCrewError::Validation(format!("PG completion state query: {}", e))
                })?;
            let Some(row) = row else {
                return Err(IronCrewError::Validation(format!(
                    "Run '{}' not found",
                    run_id
                )));
            };
            let status: String = row
                .try_get("status")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
            let parsed = status.parse::<RunStatus>()?;
            if parsed.is_terminal() {
                RunTransition::AlreadyTerminal(parsed)
            } else {
                let owner: String = row
                    .try_get("owner_instance_id")
                    .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
                return Err(IronCrewError::Validation(format!(
                    "Run '{}' is owned by instance '{}', not '{}'",
                    run_id,
                    owner,
                    self.lease.instance_id()
                )));
            }
        } else {
            RunTransition::Applied
        };

        let mapping_sql = format!(
            "UPDATE {} SET state = 'completed', lease_expires_at = '', \
             updated_at = $1, completed_at = $1, \
             expires_at = to_char(\
                 ($1::timestamptz + ttl_seconds * interval '1 second') AT TIME ZONE 'UTC', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'\
             ) \
             WHERE operation = $2 AND resource_id = $3 \
               AND state IN ('claimed', 'running', 'indeterminate')",
            self.idempotency_table
        );
        sqlx::query(sqlx::AssertSqlSafe(mapping_sql))
            .bind(&database_now)
            .bind(RUN_OPERATION)
            .bind(run_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                IronCrewError::Validation(format!(
                    "PG run idempotency completion transition: {error}"
                ))
            })?;
        self.delete_human_inputs_for_run(&mut tx, run_id).await?;
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PG update completion commit: {error}"))
        })?;
        tracing::info!("Run completion saved: {} ({})", run_id, completion.status);
        Ok(transition)
    }

    pub(in crate::engine::postgres_store) async fn set_run_status(
        &self,
        run_id: &str,
        status: RunStatus,
    ) -> Result<()> {
        if !status.is_in_flight() {
            return Err(IronCrewError::Validation(format!(
                "update_run_status requires an in-flight status, got '{}'",
                status
            )));
        }
        let sql = format!(
            "UPDATE {} SET status = $1
             WHERE run_id = $2 AND status IN ('running', 'waiting_for_input')
               AND owner_instance_id = $3",
            self.table_name
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(status.to_string())
            .bind(run_id)
            .bind(self.lease.instance_id())
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG update status: {}", e)))?;
        if result.rows_affected() == 0 {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found or not in an in-flight state",
                run_id
            )));
        }
        Ok(())
    }
}

use sqlx::Row;

use crate::engine::run_history::{ListRunsFilter, RunRecord, RunSummary};
use crate::engine::store_sql::{self, Dialect, WhereClause};
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::bind_params;
use super::super::codecs::{run_record, run_summary};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn load_run(
        &self,
        run_id: &str,
    ) -> Result<RunRecord> {
        let sql = format!(
            "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, task_results::text, agent_count, task_count, CASE WHEN octet_length(usage::text) <= 4096 THEN usage ELSE NULL END AS usage, tags::text, owner_instance_id, lease_expires_at
             FROM {} WHERE run_id = $1",
            self.table_name
        );

        let row = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?
            .ok_or_else(|| IronCrewError::Validation(format!("Run '{}' not found", run_id)))?;

        run_record(&row)
    }

    pub(in crate::engine::postgres_store) async fn load_run_summaries(
        &self,
        filter: &ListRunsFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunSummary>> {
        // Shared WHERE builder keeps the tag containment identical to the
        // SQLite backend. We NEVER select task_results — that's the whole
        // point of the summary view. LIMIT/OFFSET stay inline (trusted
        // integers) so the builder's `$N` numbering is left undisturbed.
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::runs_where(filter, Dialect::Postgres);
        let mut sql = format!(
            "SELECT run_id, flow_name, flow, status, started_at, finished_at, duration_ms, \
             agent_count, task_count, CASE WHEN octet_length(usage::text) <= 4096 THEN usage ELSE NULL END AS usage, tags::text \
             FROM {}{}",
            self.table_name, where_sql
        );
        sql.push_str(" ORDER BY started_at DESC");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit as i64));
            if offset > 0 {
                sql.push_str(&format!(" OFFSET {}", offset as i64));
            }
        }

        let query = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let query = bind_params(query, &params);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL query error: {}", e)))?;

        rows.iter().map(run_summary).collect()
    }

    pub(in crate::engine::postgres_store) async fn load_run_count(
        &self,
        filter: &ListRunsFilter,
    ) -> Result<u64> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::runs_where(filter, Dialect::Postgres);
        let sql = format!("SELECT COUNT(*) FROM {}{}", self.table_name, where_sql);

        let query = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let query = bind_params(query, &params);

        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL count error: {}", e)))?;
        let count: i64 = row
            .try_get(0)
            .map_err(|e| IronCrewError::Validation(format!("Column error: {}", e)))?;
        Ok(count as u64)
    }

    pub(in crate::engine::postgres_store) async fn remove_run(&self, run_id: &str) -> Result<()> {
        let sql = format!("DELETE FROM {} WHERE run_id = $1", self.table_name);
        let mut tx = self.pool.begin().await.map_err(|error| {
            IronCrewError::Validation(format!("PostgreSQL delete transaction: {error}"))
        })?;
        // Cascading event deletion fires the global accounting trigger. Take
        // the same lock order as append/read to prevent a run-row/usage-row
        // deadlock and keep exact counters observable throughout the delete.
        self.lock_run_event_usage(&mut tx).await?;
        let result = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(run_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PostgreSQL delete error: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(IronCrewError::Validation(format!(
                "Run '{}' not found",
                run_id
            )));
        }
        tx.commit().await.map_err(|error| {
            IronCrewError::Validation(format!("PostgreSQL delete commit: {error}"))
        })?;
        Ok(())
    }
}

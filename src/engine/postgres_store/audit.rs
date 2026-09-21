use sqlx::Row;

use crate::engine::audit::{AuditEvent, AuditFilter};
use crate::engine::store_sql::{self, Dialect, SqlParam, WhereClause};
use crate::utils::error::{IronCrewError, Result};

use super::codecs::decode_stored_json;
use super::{PostgresStore, bind_params};

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn save_audit_event_record(
        &self,
        event: &AuditEvent,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let sql = format!(
            "INSERT INTO {at}
             (id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb)",
            at = self.audit_events_table
        );
        let metadata_str = match &event.metadata {
            Some(v) => Some(
                serde_json::to_string(v)
                    .map_err(|e| IronCrewError::Validation(format!("Metadata serialize: {}", e)))?,
            ),
            None => None,
        };
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(&id)
            .bind(&event.timestamp)
            .bind(&event.action)
            .bind(&event.flow_path)
            .bind(&event.target)
            .bind(&event.actor)
            .bind(&event.source_ip)
            .bind(event.success)
            .bind(event.status_code as i32)
            .bind(metadata_str)
            .execute(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG insert audit: {}", e)))?;
        tracing::debug!("Audit event saved: {}", id);
        Ok(id)
    }

    pub(in crate::engine::postgres_store) async fn list_audit_event_records(
        &self,
        filter: &AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditEvent>> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::audit_where(filter, Dialect::Postgres);
        let mut sql = format!(
            "SELECT id, timestamp, action, flow_path, target, actor, source_ip, success, status_code, metadata::text
             FROM {}{}",
            self.audit_events_table, where_sql
        );
        sql.push_str(" ORDER BY timestamp DESC");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        if offset > 0 {
            sql.push_str(&format!(" OFFSET {}", offset));
        }

        let q = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()));
        let q = bind_params(q, &params);

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG list audit: {}", e)))?;

        let mut events = Vec::new();
        for row in rows {
            let metadata_str: Option<String> = row
                .try_get("metadata")
                .map_err(|e| IronCrewError::Validation(format!("Column error: {e}")))?;
            let metadata = metadata_str
                .as_deref()
                .map(|raw| decode_stored_json(raw, "audit_events.metadata"))
                .transpose()?;
            events.push(AuditEvent {
                id: row
                    .try_get("id")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                timestamp: row
                    .try_get("timestamp")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                action: row
                    .try_get("action")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                flow_path: row.try_get("flow_path").ok(),
                target: row.try_get("target").ok(),
                actor: row.try_get("actor").ok(),
                source_ip: row.try_get("source_ip").ok(),
                success: row
                    .try_get("success")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
                status_code: row
                    .try_get::<i32, _>("status_code")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?
                    as u16,
                metadata,
            });
        }
        Ok(events)
    }

    pub(in crate::engine::postgres_store) async fn count_audit_event_records(
        &self,
        filter: &AuditFilter,
    ) -> Result<u64> {
        let WhereClause {
            sql: where_sql,
            params,
        } = store_sql::audit_where(filter, Dialect::Postgres);
        let sql = format!(
            "SELECT COUNT(*) FROM {}{}",
            self.audit_events_table, where_sql
        );

        let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()));
        for p in &params {
            q = match p {
                SqlParam::Text(s) => q.bind(s),
                SqlParam::Bool(b) => q.bind(b),
            };
        }

        let count = q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| IronCrewError::Validation(format!("PG count audit: {}", e)))?;
        Ok(count as u64)
    }
}

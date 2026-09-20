use sqlx::Row;

use crate::engine::sessions::DialogStateRecord;
use crate::utils::error::{IronCrewError, Result};

use super::super::PostgresStore;
use super::super::codecs::decode_stored_json;

impl PostgresStore {
    pub(in crate::engine::postgres_store) async fn save_dialog_state_record(
        &self,
        record: &DialogStateRecord,
    ) -> Result<u64> {
        let agents_json = serde_json::to_string(&record.agent_names).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize agent_names: {}", e))
        })?;
        let transcript_json = serde_json::to_string(&record.transcript).map_err(|e| {
            IronCrewError::Validation(format!("Failed to serialize transcript: {}", e))
        })?;
        let expected_revision = i64::try_from(record.revision)
            .map_err(|_| IronCrewError::Validation("Dialog revision is out of range".into()))?;
        let mut tx = self.pool.begin().await.map_err(|e| {
            IronCrewError::Validation(format!(
                "PostgreSQL save_dialog_state transaction error: {e}"
            ))
        })?;
        let select_sql = format!(
            "SELECT revision FROM {} \
             WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 FOR UPDATE",
            self.dialogs_table
        );
        let current: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(select_sql))
            .bind(&record.id)
            .bind(&record.flow_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!(
                    "PostgreSQL save_dialog_state revision read error: {e}"
                ))
            })?;
        let revision: Option<i64> = match current {
            None if expected_revision == 0 => {
                let insert_sql = format!(
                    "INSERT INTO {} \
                     (id, flow_name, flow_path, agent_names, starter, transcript, next_index, stopped, stop_reason, created_at, updated_at, revision) \
                     VALUES ($1, $2, $3, $4::jsonb, $5, $6::jsonb, $7, $8, $9, $10, $11, 1) \
                     ON CONFLICT (flow_path, id) DO NOTHING RETURNING revision",
                    self.dialogs_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(insert_sql))
                    .bind(&record.id)
                    .bind(&record.flow_name)
                    .bind(&record.flow_path)
                    .bind(&agents_json)
                    .bind(&record.starter)
                    .bind(&transcript_json)
                    .bind(record.next_index as i32)
                    .bind(record.stopped)
                    .bind(&record.stop_reason)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_dialog_state insert error: {e}"
                        ))
                    })?
            }
            Some(current) if current == expected_revision => {
                let update_sql = format!(
                    "UPDATE {} SET flow_name = $3, agent_names = $4::jsonb, \
                     starter = $5, transcript = $6::jsonb, next_index = $7, \
                     stopped = $8, stop_reason = $9, created_at = $10, \
                     updated_at = $11, revision = revision + 1 \
                     WHERE id = $1 AND flow_path IS NOT DISTINCT FROM $2 AND revision = $12 \
                     RETURNING revision",
                    self.dialogs_table
                );
                sqlx::query_scalar(sqlx::AssertSqlSafe(update_sql))
                    .bind(&record.id)
                    .bind(&record.flow_path)
                    .bind(&record.flow_name)
                    .bind(&agents_json)
                    .bind(&record.starter)
                    .bind(&transcript_json)
                    .bind(record.next_index as i32)
                    .bind(record.stopped)
                    .bind(&record.stop_reason)
                    .bind(&record.created_at)
                    .bind(&record.updated_at)
                    .bind(expected_revision)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| {
                        IronCrewError::Validation(format!(
                            "PostgreSQL save_dialog_state update error: {e}"
                        ))
                    })?
            }
            _ => None,
        };
        let revision = revision.ok_or_else(|| {
            IronCrewError::Conflict(format!(
                "Dialog '{}' changed since revision {}; reopen it before saving",
                record.id, record.revision
            ))
        })?;
        tx.commit().await.map_err(|e| {
            IronCrewError::Validation(format!("PostgreSQL save_dialog_state commit error: {e}"))
        })?;
        u64::try_from(revision)
            .map_err(|_| IronCrewError::Validation("Invalid dialog revision".into()))
    }

    pub(in crate::engine::postgres_store) async fn get_dialog_state_record(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<Option<DialogStateRecord>> {
        let sql = format!(
            "SELECT id, flow_name, flow_path, agent_names::text, starter, transcript::text, \
             next_index, stopped, stop_reason, created_at, updated_at, revision \
             FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.dialogs_table
        );
        let row_opt = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL get_dialog_state error: {}", e))
            })?;
        let Some(row) = row_opt else {
            return Ok(None);
        };
        let agents_str: String = row
            .try_get("agent_names")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        let transcript_str: String = row
            .try_get("transcript")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        let next_index_i32: i32 = row
            .try_get("next_index")
            .map_err(|e| IronCrewError::Validation(e.to_string()))?;
        Ok(Some(DialogStateRecord {
            id: row
                .try_get("id")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_name: row
                .try_get("flow_name")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            flow_path: row
                .try_get("flow_path")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            agent_names: decode_stored_json(&agents_str, "dialogs.agent_names")?,
            starter: row
                .try_get("starter")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            transcript: decode_stored_json(&transcript_str, "dialogs.transcript")?,
            next_index: next_index_i32.max(0) as usize,
            stopped: row
                .try_get("stopped")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            stop_reason: row
                .try_get("stop_reason")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            created_at: row
                .try_get("created_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            updated_at: row
                .try_get("updated_at")
                .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            revision: u64::try_from(
                row.try_get::<i64, _>("revision")
                    .map_err(|e| IronCrewError::Validation(e.to_string()))?,
            )
            .map_err(|_| IronCrewError::Validation("Invalid dialog revision".into()))?,
        }))
    }

    pub(in crate::engine::postgres_store) async fn delete_dialog_state_record(
        &self,
        flow_path: Option<&str>,
        id: &str,
    ) -> Result<()> {
        let sql = format!(
            "DELETE FROM {} WHERE id = $1 AND ($2::TEXT IS NULL OR flow_path = $2)",
            self.dialogs_table
        );
        sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .bind(flow_path)
            .execute(&self.pool)
            .await
            .map_err(|e| {
                IronCrewError::Validation(format!("PostgreSQL delete_dialog_state error: {}", e))
            })?;
        Ok(())
    }
}

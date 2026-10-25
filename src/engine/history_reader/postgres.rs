use std::str::FromStr;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sqlx::{ConnectOptions, Connection, PgConnection, Row, postgres::PgConnectOptions};

use super::{HistoryRecord, MAX_RECORD_BYTES, decode_history_record, invalid};
use crate::utils::error::Result;

const MAX_LIST_LIMIT: usize = 100;
const MAX_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_OFFSET: usize = 10_000;

/// A connection that never initializes a runtime store or performs maintenance.
pub struct PostgresHistoryReader {
    connection: PgConnection,
    table: String,
}

#[derive(Debug, Serialize)]
pub struct HistoryPage {
    pub read_only: bool,
    pub runs: Vec<Value>,
    pub limit: usize,
    pub offset: usize,
    pub has_more: bool,
}

impl PostgresHistoryReader {
    /// Use a SELECT-only role. Session and transaction policy also enforce read-only access.
    pub async fn connect(database_url: &str, prefix: &str) -> Result<Self> {
        if prefix.len() > 37
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(invalid(
                "History table prefix must be at most 37 lowercase ASCII identifier bytes",
            ));
        }
        let options = PgConnectOptions::from_str(database_url)
            .map_err(|_| invalid("History database URL is invalid"))?
            .application_name("ironcrew-history-reader")
            .disable_statement_logging()
            .options([
                ("default_transaction_read_only", "on"),
                ("statement_timeout", "10000"),
                ("lock_timeout", "1000"),
                ("idle_in_transaction_session_timeout", "10000"),
            ]);
        let mut connection = tokio::time::timeout(
            Duration::from_secs(10),
            PgConnection::connect_with(&options),
        )
        .await
        .map_err(|_| invalid("History database connection timed out"))?
        .map_err(|_| invalid("History database connection failed"))?;
        let version: String = sqlx::query_scalar("SHOW server_version_num")
            .fetch_one(&mut connection)
            .await
            .map_err(|_| invalid("History database version could not be read"))?;
        if version.parse::<u32>().unwrap_or(0) < 170_000 {
            return Err(invalid("History reader requires PostgreSQL 17 or newer"));
        }
        Ok(Self {
            connection,
            table: format!("public.\"{prefix}runs\""),
        })
    }

    /// Read a bounded metadata page without interpreting counters as checked usage.
    pub async fn list(
        &mut self,
        flow: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<HistoryPage> {
        if !(1..=MAX_LIST_LIMIT).contains(&limit) || offset > MAX_OFFSET {
            return Err(invalid(
                "History list requires limit 1..100 and offset 0..10000",
            ));
        }
        validate_flow(flow)?;
        let sql = format!(
            "SELECT CASE WHEN pg_catalog.octet_length(summary::text) <= {MAX_SUMMARY_BYTES} \
             THEN summary::text END AS summary FROM \
             (SELECT pg_catalog.json_build_object('run_id',run_id,'flow',flow, \
             'flow_name',flow_name,'status',status,'started_at',started_at, \
             'finished_at',finished_at,'agent_count',agent_count,'task_count',task_count) AS summary \
             FROM {} WHERE ($1::text IS NULL OR flow=$1) \
             ORDER BY started_at DESC,run_id DESC LIMIT $2 OFFSET $3) page",
            self.table
        );
        let mut tx = self
            .connection
            .begin_with("BEGIN READ ONLY")
            .await
            .map_err(|_| invalid("History read-only transaction could not start"))?;
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(flow)
            .bind((limit + 1) as i64)
            .bind(offset as i64)
            .fetch_all(&mut *tx)
            .await
            .map_err(|_| invalid("History list query failed"))?;
        let mut runs = Vec::with_capacity(limit);
        for row in rows.iter().take(limit) {
            let raw: Option<String> = row
                .try_get("summary")
                .map_err(|_| invalid("Historical summary could not be read"))?;
            runs.push(
                serde_json::from_str(
                    &raw.ok_or_else(|| invalid("Historical summary exceeds 16 KiB"))?,
                )
                .map_err(|_| invalid("Historical summary is invalid"))?,
            );
        }
        tx.rollback()
            .await
            .map_err(|_| invalid("History read-only transaction could not close"))?;
        Ok(HistoryPage {
            read_only: true,
            has_more: rows.len() > limit,
            runs,
            limit,
            offset,
        })
    }

    /// Return the original row with a separate inspection-only checked-usage projection.
    pub async fn inspect(&mut self, id: &str, flow: Option<&str>) -> Result<HistoryRecord> {
        if id.is_empty() || id.len() > 255 || id.chars().any(char::is_control) {
            return Err(invalid(
                "History run ID must contain 1..255 non-control bytes",
            ));
        }
        validate_flow(flow)?;
        let sql = format!(
            "SELECT CASE WHEN pg_catalog.octet_length(pg_catalog.row_to_json(r)::text) <= $2 \
             THEN pg_catalog.row_to_json(r)::text END AS record \
             FROM {} r WHERE run_id=$1 AND ($3::text IS NULL OR flow=$3)",
            self.table
        );
        let mut tx = self
            .connection
            .begin_with("BEGIN READ ONLY")
            .await
            .map_err(|_| invalid("History read-only transaction could not start"))?;
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .bind(MAX_RECORD_BYTES as i32)
            .bind(flow)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| invalid("History inspect query failed"))?
            .ok_or_else(|| invalid("Historical run not found"))?;
        let raw: Option<String> = row
            .try_get("record")
            .map_err(|_| invalid("Historical record could not be read"))?;
        let raw = raw.ok_or_else(|| invalid("Historical record exceeds 8 MiB"))?;
        tx.rollback()
            .await
            .map_err(|_| invalid("History read-only transaction could not close"))?;
        // Parsing happens on a blocking worker; raw bytes were bounded by SQL before transport.
        tokio::task::spawn_blocking(move || decode_history_record(&raw))
            .await
            .map_err(|_| invalid("Historical record decoder failed"))?
    }
}

fn validate_flow(flow: Option<&str>) -> Result<()> {
    if flow.is_some_and(|flow| flow.len() > 255 || flow.chars().any(char::is_control)) {
        return Err(invalid(
            "History flow must contain at most 255 non-control bytes",
        ));
    }
    Ok(())
}

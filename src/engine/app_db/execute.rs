//! Transactional execution of declared operations with bounded results.

use futures::TryStreamExt;
use sqlx::postgres::PgRow;
use sqlx::{Column, Postgres, Row, TypeInfo};

use super::AppDb;
use super::operations::{Operation, ParamType};
use crate::utils::error::{IronCrewError, Result};

fn op_error(name: &str, message: impl std::fmt::Display) -> IronCrewError {
    IronCrewError::Validation(format!("postgres operation '{name}': {message}"))
}

fn bind_params<'q>(
    mut query: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    operation: &'q Operation,
    params: &'q [serde_json::Value],
    bind_count: usize,
) -> Result<sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>> {
    for ((param_name, param_type), value) in
        operation.params.iter().zip(params.iter()).take(bind_count)
    {
        let mismatch = |expected: &str| {
            op_error(
                &operation.name,
                format!("param '{param_name}' expects {expected}"),
            )
        };
        query = match (param_type, value) {
            (_, serde_json::Value::Null) => match param_type {
                ParamType::Text => query.bind(None::<String>),
                ParamType::Integer => query.bind(None::<i64>),
                ParamType::Double => query.bind(None::<f64>),
                ParamType::Boolean => query.bind(None::<bool>),
                ParamType::Json => query.bind(None::<serde_json::Value>),
            },
            (ParamType::Text, serde_json::Value::String(s)) => query.bind(s.clone()),
            (ParamType::Integer, serde_json::Value::Number(n)) => {
                query.bind(n.as_i64().ok_or_else(|| mismatch("an integer"))?)
            }
            (ParamType::Double, serde_json::Value::Number(n)) => {
                query.bind(n.as_f64().ok_or_else(|| mismatch("a number"))?)
            }
            (ParamType::Boolean, serde_json::Value::Bool(b)) => query.bind(*b),
            (ParamType::Json, any) => query.bind(any.clone()),
            (expected, _) => return Err(mismatch(expected.name())),
        };
    }
    Ok(query)
}

fn decode_row(operation_name: &str, row: &PgRow) -> Result<serde_json::Value> {
    let mut object = serde_json::Map::new();
    for (index, column) in row.columns().iter().enumerate() {
        let type_name = column.type_info().name().to_uppercase();
        let value = match type_name.as_str() {
            "TEXT" | "VARCHAR" | "BPCHAR" | "CHAR" | "NAME" => row
                .try_get::<Option<String>, _>(index)
                .map(|v| v.map_or(serde_json::Value::Null, serde_json::Value::String)),
            "INT2" => row
                .try_get::<Option<i16>, _>(index)
                .map(|v| v.map_or(serde_json::Value::Null, |n| n.into())),
            "INT4" => row
                .try_get::<Option<i32>, _>(index)
                .map(|v| v.map_or(serde_json::Value::Null, |n| n.into())),
            "INT8" => row
                .try_get::<Option<i64>, _>(index)
                .map(|v| v.map_or(serde_json::Value::Null, |n| n.into())),
            "FLOAT4" => row
                .try_get::<Option<f32>, _>(index)
                .map(|v| v.map_or(serde_json::Value::Null, |n| serde_json::json!(n))),
            "FLOAT8" => row
                .try_get::<Option<f64>, _>(index)
                .map(|v| v.map_or(serde_json::Value::Null, |n| serde_json::json!(n))),
            "BOOL" => row
                .try_get::<Option<bool>, _>(index)
                .map(|v| v.map_or(serde_json::Value::Null, serde_json::Value::Bool)),
            "JSON" | "JSONB" => row
                .try_get::<Option<serde_json::Value>, _>(index)
                .map(|v| v.unwrap_or(serde_json::Value::Null)),
            other => {
                return Err(op_error(
                    operation_name,
                    format!(
                        "column '{}' has unsupported type {other}; cast it in SQL (e.g. ::text)",
                        column.name()
                    ),
                ));
            }
        }
        .map_err(|e| op_error(operation_name, format!("decode '{}': {e}", column.name())))?;
        object.insert(column.name().to_string(), value);
    }
    Ok(serde_json::Value::Object(object))
}

fn validate_call<'a>(
    app: &'a AppDb,
    name: &str,
    params: &[serde_json::Value],
) -> Result<&'a Operation> {
    let operation = app.operation(name)?;
    if params.len() != operation.params.len() {
        return Err(op_error(
            name,
            format!(
                "expects {} param(s), got {}",
                operation.params.len(),
                params.len()
            ),
        ));
    }
    Ok(operation)
}

async fn begin_with_timeout<'a>(
    app: &'a AppDb,
    name: &str,
) -> Result<sqlx::Transaction<'a, Postgres>> {
    let pool = app.pool().await?;
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| op_error(name, format!("begin failed: {e}")))?;
    // Trusted integer from the captured policy; SET LOCAL scopes it to this
    // transaction so the bound travels with the work.
    let timeout = app.policy().statement_timeout_ms();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {timeout}"
    )))
    .execute(&mut *tx)
    .await
    .map_err(|e| op_error(name, format!("statement_timeout setup failed: {e}")))?;
    Ok(tx)
}

pub(super) async fn run_execute(
    app: &AppDb,
    name: &str,
    params: &[serde_json::Value],
) -> Result<u64> {
    let operation = validate_call(app, name, params)?;
    let mut tx = begin_with_timeout(app, name).await?;
    let mut total = 0u64;
    for statement in &operation.statements {
        let query = bind_params(
            sqlx::query(sqlx::AssertSqlSafe(statement.sql.as_str())),
            operation,
            params,
            statement.bind_count,
        )?;
        let result = query
            .execute(&mut *tx)
            .await
            .map_err(|e| op_error(name, e))?;
        total = total.saturating_add(result.rows_affected());
    }
    tx.commit()
        .await
        .map_err(|e| op_error(name, format!("commit failed: {e}")))?;
    Ok(total)
}

pub(super) async fn run_query(
    app: &AppDb,
    name: &str,
    params: &[serde_json::Value],
) -> Result<Vec<serde_json::Value>> {
    let max_rows = app.policy().max_rows();
    run_query_bounded(app, name, params, max_rows.saturating_add(1))
        .await
        .and_then(|rows| {
            if rows.len() > max_rows {
                return Err(op_error(
                    name,
                    format!("result exceeds IRONCREW_APP_DB_MAX_ROWS ({max_rows}); add a LIMIT"),
                ));
            }
            Ok(rows)
        })
}

pub(super) async fn run_query_bounded(
    app: &AppDb,
    name: &str,
    params: &[serde_json::Value],
    fetch_cap: usize,
) -> Result<Vec<serde_json::Value>> {
    let operation = validate_call(app, name, params)?;
    if operation.statements.len() != 1 {
        return Err(op_error(
            name,
            "query operations must contain exactly one statement",
        ));
    }
    let statement = &operation.statements[0];
    let mut tx = begin_with_timeout(app, name).await?;
    let max_bytes = app.policy().max_response_bytes();
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    {
        let query = bind_params(
            sqlx::query(sqlx::AssertSqlSafe(statement.sql.as_str())),
            operation,
            params,
            statement.bind_count,
        )?;
        let mut stream = query.fetch(&mut *tx);
        while let Some(row) = stream.try_next().await.map_err(|e| op_error(name, e))? {
            if rows.len() >= fetch_cap {
                break;
            }
            let value = decode_row(name, &row)?;
            bytes = bytes.saturating_add(
                serde_json::to_string(&value)
                    .map(|s| s.len())
                    .unwrap_or(usize::MAX),
            );
            if bytes > max_bytes {
                return Err(op_error(
                    name,
                    format!("result exceeds IRONCREW_APP_DB_MAX_RESPONSE_BYTES ({max_bytes})"),
                ));
            }
            rows.push(value);
        }
    }
    tx.commit()
        .await
        .map_err(|e| op_error(name, format!("commit failed: {e}")))?;
    Ok(rows)
}

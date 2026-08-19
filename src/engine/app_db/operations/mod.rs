//! Named-operation declarations: `sql/*.sql` parsing and the registry.

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::policy::AppDbPolicy;
use super::sql_split::split_statements;
use crate::utils::error::{IronCrewError, Result};

mod header;

use header::{op_error, parse_params, validate_op_name};

const MAX_OP_NAME_BYTES: usize = 128;
const MAX_PARAMS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ParamType {
    Text,
    Integer,
    Double,
    Boolean,
    Json,
}

impl ParamType {
    #[allow(dead_code)]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "integer" => Some(Self::Integer),
            "double" => Some(Self::Double),
            "boolean" => Some(Self::Boolean),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Integer => "integer",
            Self::Double => "double",
            Self::Boolean => "boolean",
            Self::Json => "json",
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Statement {
    pub sql: String,
    /// Number of params to bind: the highest `$n` this statement references.
    pub bind_count: usize,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Operation {
    pub name: String,
    pub params: Vec<(String, ParamType)>,
    pub statements: Vec<Statement>,
    pub digest: String,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct OperationRegistry {
    operations: BTreeMap<String, Operation>,
}

#[allow(dead_code)]
fn parse_operation(name: &str, source: &str) -> Result<Operation> {
    let mut lines = source.lines();
    let marker = lines.next().map(str::trim);
    if marker != Some("-- ironcrew:op") {
        return Err(op_error(
            name,
            "first line must be exactly '-- ironcrew:op'",
        ));
    }

    let mut params = Vec::new();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut in_header = true;
    let mut saw_params_line = false;
    for line in lines {
        let trimmed = line.trim_start();
        if in_header && trimmed.starts_with("--") {
            if let Some(rest) = trimmed
                .trim_start_matches("--")
                .trim_start()
                .strip_prefix("params:")
            {
                if saw_params_line {
                    return Err(op_error(name, "duplicate '-- params:' line"));
                }
                saw_params_line = true;
                params = parse_params(name, rest)?;
            }
            continue;
        }
        in_header = false;
        body_lines.push(line);
    }

    let body = body_lines.join("\n");
    let statements = split_statements(&body).map_err(|message| op_error(name, message))?;
    if statements.is_empty() {
        return Err(op_error(name, "operation body contains no SQL statements"));
    }

    let mut converted = Vec::with_capacity(statements.len());
    for statement in statements {
        if statement.max_placeholder > params.len() {
            return Err(op_error(
                name,
                format!(
                    "statement references ${} but only {} params are declared",
                    statement.max_placeholder,
                    params.len()
                ),
            ));
        }
        converted.push(Statement {
            sql: statement.sql,
            bind_count: statement.max_placeholder,
        });
    }

    let digest = {
        let mut hasher = Sha256::new();
        hasher.update(source.as_bytes());
        let hex: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("sha256:{hex}")
    };

    Ok(Operation {
        name: name.to_string(),
        params,
        statements: converted,
        digest,
    })
}

impl OperationRegistry {
    #[allow(dead_code)]
    pub fn from_sources(sources: Vec<(String, String)>, policy: &AppDbPolicy) -> Result<Self> {
        if sources.len() > policy.max_operations() {
            return Err(IronCrewError::Validation(format!(
                "{} postgres operations exceed IRONCREW_APP_DB_MAX_OPERATIONS ({})",
                sources.len(),
                policy.max_operations()
            )));
        }
        let mut operations = BTreeMap::new();
        for (name, source) in sources {
            validate_op_name(&name)?;
            if source.len() > policy.max_sql_bytes() {
                return Err(op_error(
                    &name,
                    format!(
                        "source is {} bytes, exceeds IRONCREW_APP_DB_MAX_SQL_BYTES ({})",
                        source.len(),
                        policy.max_sql_bytes()
                    ),
                ));
            }
            let operation = parse_operation(&name, &source)?;
            if operations.insert(name.clone(), operation).is_some() {
                return Err(op_error(&name, "duplicate operation name"));
            }
        }
        Ok(Self { operations })
    }

    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&Operation> {
        self.operations.get(name)
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Sorted, non-secret description for the drift fingerprint.
    #[allow(dead_code)]
    pub fn definition(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.operations
                .values()
                .map(|op| {
                    serde_json::json!({
                        "name": op.name,
                        "digest": op.digest,
                        "params": op
                            .params
                            .iter()
                            .map(|(name, ty)| serde_json::json!({"name": name, "type": ty.name()}))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    }
}

/// Read `<project>/sql/*.sql` (non-recursive, sorted). Missing dir → empty.
#[allow(dead_code)]
pub fn read_sql_dir(project_dir: &Path, policy: &AppDbPolicy) -> Result<Vec<(String, String)>> {
    let sql_dir = project_dir.join("sql");
    if !sql_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&sql_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sql") && path.is_file() {
            names.push(path);
        }
    }
    names.sort();
    if names.len() > policy.max_operations() {
        return Err(IronCrewError::Validation(format!(
            "{} files in sql/ exceed IRONCREW_APP_DB_MAX_OPERATIONS ({})",
            names.len(),
            policy.max_operations()
        )));
    }
    let mut sources = Vec::with_capacity(names.len());
    for path in names {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                IronCrewError::Validation(format!("invalid sql file name: {}", path.display()))
            })?
            .to_string();
        let bytes = std::fs::metadata(&path)?.len();
        if bytes > policy.max_sql_bytes() as u64 {
            return Err(op_error(
                &name,
                format!(
                    "file exceeds IRONCREW_APP_DB_MAX_SQL_BYTES ({})",
                    policy.max_sql_bytes()
                ),
            ));
        }
        sources.push((name, std::fs::read_to_string(&path)?));
    }
    Ok(sources)
}

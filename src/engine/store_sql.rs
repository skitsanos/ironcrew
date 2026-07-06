//! Shared SQL WHERE-clause builders for the SQLite and Postgres backends.
//!
//! The two backends differ only in placeholder syntax (`?N` vs `$N`) and in
//! how a few operators spell out per dialect (notably JSON containment for the
//! tag filter). Building the clause once here keeps `list`/`count` in sync with
//! each other and keeps the two backends from drifting — which is exactly how
//! the old hand-copied tag filter ended up matching differently on each engine.
//!
//! Each builder returns the clause string (including the leading `WHERE`, or
//! empty) plus the ordered parameter values. Callers bind the values with their
//! own driver mechanism (`rusqlite::params_from_iter` / `sqlx::query::bind`).
//! `LIMIT`/`OFFSET` are intentionally left to the caller as integer literals
//! (trusted `usize`), so there's no cross-builder placeholder bookkeeping.

use crate::engine::audit::AuditFilter;
use crate::engine::run_history::ListRunsFilter;

/// Which SQL engine the clause is being built for. Governs placeholder syntax
/// and dialect-specific operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

/// A bound parameter value. Both drivers can bind these: rusqlite via `ToSql`
/// (impl'd for `String`/`i64`), sqlx via `Encode` for the matching column type.
#[derive(Debug, Clone)]
pub enum SqlParam {
    Text(String),
    /// Bound as a boolean. Each backend maps it to its own `success` column
    /// type — an integer 0/1 in SQLite, a native `BOOLEAN` in Postgres.
    Bool(bool),
}

/// A built WHERE clause plus its ordered bind values.
pub struct WhereClause {
    /// The clause including a leading `" WHERE "`, or empty when no filter is
    /// active. Placeholders are numbered from 1 in the dialect's syntax.
    pub sql: String,
    pub params: Vec<SqlParam>,
}

impl Dialect {
    /// Render the 1-based placeholder for parameter `n` in this dialect.
    fn placeholder(self, n: usize) -> String {
        match self {
            Dialect::Sqlite => format!("?{n}"),
            Dialect::Postgres => format!("${n}"),
        }
    }
}

/// Build the WHERE clause for `list_runs_summary` / `count_runs`.
///
/// Flow scope, status, `started_at >= since`, and tag containment. The tag
/// clause is dialect-specific but semantically identical: an exact element
/// match against the JSON `tags` array (no `LIKE '%"tag"%'`, which mis-handled
/// `%`/`_`/quotes and drifted between backends).
pub fn runs_where(filter: &ListRunsFilter, dialect: Dialect) -> WhereClause {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();
    let mut n = 0usize;

    if let Some(ref flow) = filter.flow {
        n += 1;
        clauses.push(format!("flow = {}", dialect.placeholder(n)));
        params.push(SqlParam::Text(flow.clone()));
    }
    if let Some(ref status) = filter.status {
        n += 1;
        clauses.push(format!("status = {}", dialect.placeholder(n)));
        params.push(SqlParam::Text(status.clone()));
    }
    if let Some(ref since) = filter.since {
        n += 1;
        clauses.push(format!("started_at >= {}", dialect.placeholder(n)));
        params.push(SqlParam::Text(since.clone()));
    }
    if let Some(ref tag) = filter.tag {
        n += 1;
        let ph = dialect.placeholder(n);
        match dialect {
            // Exact element match over the JSON array, so a tag can contain
            // `%`, `_`, or quotes without breaking the query or matching a
            // substring of a different tag.
            Dialect::Sqlite => {
                clauses.push(format!(
                    "EXISTS (SELECT 1 FROM json_each(tags) WHERE value = {ph})"
                ));
                params.push(SqlParam::Text(tag.clone()));
            }
            // jsonb containment against a single-element array. The array is
            // serialized with serde so embedded quotes/backslashes are escaped.
            Dialect::Postgres => {
                clauses.push(format!("tags @> {ph}::jsonb"));
                let json = serde_json::to_string(std::slice::from_ref(tag))
                    .unwrap_or_else(|_| "[]".to_string());
                params.push(SqlParam::Text(json));
            }
        }
    }

    finish(clauses, params)
}

/// Build the WHERE clause for `list_audit_events` / `count_audit_events`.
pub fn audit_where(filter: &AuditFilter, dialect: Dialect) -> WhereClause {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<SqlParam> = Vec::new();
    let mut n = 0usize;

    // Equality filters over text columns, in a stable order so placeholder
    // numbers line up with the pushed params.
    for (col, val) in [
        ("flow_path", &filter.flow_path),
        ("action", &filter.action),
        ("actor", &filter.actor),
    ] {
        if let Some(v) = val {
            n += 1;
            clauses.push(format!("{col} = {}", dialect.placeholder(n)));
            params.push(SqlParam::Text(v.clone()));
        }
    }
    if let Some(ref since) = filter.since {
        n += 1;
        clauses.push(format!("timestamp >= {}", dialect.placeholder(n)));
        params.push(SqlParam::Text(since.clone()));
    }
    if let Some(ref until) = filter.until {
        n += 1;
        clauses.push(format!("timestamp < {}", dialect.placeholder(n)));
        params.push(SqlParam::Text(until.clone()));
    }
    if let Some(success) = filter.success {
        n += 1;
        clauses.push(format!("success = {}", dialect.placeholder(n)));
        params.push(SqlParam::Bool(success));
    }

    finish(clauses, params)
}

fn finish(clauses: Vec<String>, params: Vec<SqlParam>) -> WhereClause {
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    WhereClause { sql, params }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_yields_no_clause() {
        let wc = runs_where(&ListRunsFilter::default(), Dialect::Sqlite);
        assert!(wc.sql.is_empty());
        assert!(wc.params.is_empty());
    }

    #[test]
    fn flow_and_status_number_placeholders_per_dialect() {
        let filter = ListRunsFilter {
            flow: Some("research".into()),
            status: Some("success".into()),
            ..Default::default()
        };
        let sqlite = runs_where(&filter, Dialect::Sqlite);
        assert_eq!(sqlite.sql, " WHERE flow = ?1 AND status = ?2");
        let pg = runs_where(&filter, Dialect::Postgres);
        assert_eq!(pg.sql, " WHERE flow = $1 AND status = $2");
        assert_eq!(sqlite.params.len(), 2);
    }

    #[test]
    fn tag_filter_is_dialect_specific_containment() {
        let filter = ListRunsFilter {
            tag: Some("v2".into()),
            ..Default::default()
        };
        let sqlite = runs_where(&filter, Dialect::Sqlite);
        assert!(sqlite.sql.contains("json_each(tags)"));
        assert!(!sqlite.sql.contains("LIKE"));
        let pg = runs_where(&filter, Dialect::Postgres);
        assert!(pg.sql.contains("tags @> $1::jsonb"));
        // The Postgres tag value is a serialized single-element JSON array.
        match &pg.params[0] {
            SqlParam::Text(s) => assert_eq!(s, "[\"v2\"]"),
            _ => panic!("expected text param"),
        }
    }

    #[test]
    fn tag_with_special_chars_is_safely_serialized() {
        let filter = ListRunsFilter {
            tag: Some("a\"b%c".into()),
            ..Default::default()
        };
        let pg = runs_where(&filter, Dialect::Postgres);
        match &pg.params[0] {
            SqlParam::Text(s) => assert_eq!(s, "[\"a\\\"b%c\"]"),
            _ => panic!("expected text param"),
        }
    }

    #[test]
    fn audit_filter_mixes_text_and_int_params() {
        let filter = AuditFilter {
            action: Some("flow.run.start".into()),
            success: Some(false),
            ..Default::default()
        };
        let wc = audit_where(&filter, Dialect::Postgres);
        assert_eq!(wc.sql, " WHERE action = $1 AND success = $2");
        assert!(matches!(wc.params[1], SqlParam::Bool(false)));
    }
}

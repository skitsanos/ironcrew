use std::collections::HashMap;

use crate::engine::task::TaskResult;

/// Interpolate `${results.task_name.field}` patterns in a string.
///
/// Supported paths:
/// - `${results.task_name.output}` — the output text of a completed task
/// - `${results.task_name.success}` — "true" or "false"
/// - `${results.task_name.agent}` — the agent that handled the task
/// - `${results.task_name.duration_ms}` — execution time in ms
/// - `${env.VAR_NAME}` — environment variable
///
/// Unresolved patterns are replaced with empty string.
pub fn interpolate(template: &str, results: &HashMap<String, TaskResult>) -> String {
    let mut output = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut expr = String::new();
            let mut depth = 1;
            for c in chars.by_ref() {
                if c == '{' {
                    depth += 1;
                    expr.push(c);
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    expr.push(c);
                } else {
                    expr.push(c);
                }
            }
            let resolved = resolve_expression(&expr, results);
            output.push_str(&resolved);
        } else {
            output.push(ch);
        }
    }

    output
}

fn resolve_expression(expr: &str, results: &HashMap<String, TaskResult>) -> String {
    let parts: Vec<&str> = expr.trim().splitn(3, '.').collect();

    match parts.as_slice() {
        ["results", task_name, field] => {
            if let Some(result) = results.get(*task_name) {
                match *field {
                    "output" => result.output.clone(),
                    "success" => result.success.to_string(),
                    "agent" => result.agent.clone(),
                    "duration_ms" => result.duration_ms.to_string(),
                    "task" => result.task.clone(),
                    _ => String::new(),
                }
            } else {
                String::new()
            }
        }
        ["env", var_name] => std::env::var(var_name).unwrap_or_default(),
        _ => String::new(),
    }
}

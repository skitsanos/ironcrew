use std::collections::HashMap;

use crate::engine::task::TaskResult;

const DEFAULT_MAX_PROMPT_CHARS: usize = 100 * 1024;
const HARD_MAX_PROMPT_CHARS: usize = 4 * 1024 * 1024;

pub fn prompt_char_limit() -> usize {
    std::env::var("IRONCREW_MAX_PROMPT_CHARS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(HARD_MAX_PROMPT_CHARS))
        .unwrap_or(DEFAULT_MAX_PROMPT_CHARS)
}

struct BoundedInterpolation {
    output: String,
    max_chars: usize,
    chars: usize,
    truncated: bool,
}

impl BoundedInterpolation {
    fn new(max_chars: usize, template_bytes: usize) -> Self {
        Self {
            output: String::with_capacity(template_bytes.min(max_chars).min(16 * 1024)),
            max_chars,
            chars: 0,
            truncated: false,
        }
    }

    fn push(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        if self.chars >= self.max_chars {
            self.truncated = true;
            return;
        }
        let remaining = self.max_chars - self.chars;
        let mut included = 0usize;
        let mut boundary = value.len();
        for (byte_index, _) in value.char_indices() {
            if included == remaining {
                boundary = byte_index;
                self.truncated = true;
                break;
            }
            included += 1;
        }
        self.output.push_str(&value[..boundary]);
        self.chars += included.min(remaining);
    }

    fn finish(self) -> (String, bool) {
        (self.output, self.truncated)
    }
}

/// Interpolate `${results.task_name.field}` patterns in a string.
///
/// Supported paths:
/// - `${results.task_name.output}` — the output text of a completed task
/// - `${results.task_name.success}` — "true" or "false"
/// - `${results.task_name.agent}` — the agent that handled the task
/// - `${results.task_name.duration_ms}` — execution time in ms
/// - `${env.VAR_NAME}` — environment variable, only when explicitly named in
///   `IRONCREW_ENV_ALLOWLIST`
///
/// Unresolved patterns are replaced with empty string.
pub fn interpolate(template: &str, results: &HashMap<String, TaskResult>) -> String {
    interpolate_bounded(template, results, prompt_char_limit()).0
}

/// Interpolate into a character-bounded buffer. Dependency values are copied
/// only up to the remaining budget, so repeating a large `${results.*}` token
/// cannot allocate an amplified temporary before prompt truncation.
pub fn interpolate_bounded(
    template: &str,
    results: &HashMap<String, TaskResult>,
    max_chars: usize,
) -> (String, bool) {
    let mut output = BoundedInterpolation::new(max_chars, template.len());
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
            append_expression(&expr, results, &mut output);
        } else {
            let mut encoded = [0u8; 4];
            output.push(ch.encode_utf8(&mut encoded));
        }
    }

    output.finish()
}

fn append_expression(
    expr: &str,
    results: &HashMap<String, TaskResult>,
    output: &mut BoundedInterpolation,
) {
    let parts: Vec<&str> = expr.trim().splitn(3, '.').collect();

    match parts.as_slice() {
        ["results", task_name, field] => {
            if let Some(result) = results.get(*task_name) {
                match *field {
                    "output" => output.push(&result.output),
                    "success" => output.push(if result.success { "true" } else { "false" }),
                    "agent" => output.push(&result.agent),
                    "duration_ms" => output.push(&result.duration_ms.to_string()),
                    "task" => output.push(&result.task),
                    _ => {}
                }
            }
        }
        ["env", var_name] => {
            if let Some(value) = crate::utils::env::read_allowlisted(var_name) {
                output.push(&value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;

    #[test]
    fn repeated_large_result_never_exceeds_expansion_budget() {
        let mut results = HashMap::new();
        results.insert(
            "large".into(),
            TaskResult {
                task: "large".into(),
                agent: "agent".into(),
                output: "é".repeat(1024 * 1024),
                success: true,
                duration_ms: 1,
                token_usage: None,
                reasoning: None,
            },
        );
        let template = "${results.large.output}".repeat(10_000);
        let (expanded, truncated) = interpolate_bounded(&template, &results, 4096);
        assert!(truncated);
        assert_eq!(expanded.chars().count(), 4096);
        assert_eq!(expanded.len(), 8192);
    }
}

use super::{RetainedResultBudget, serialized_result_bytes};
use crate::engine::task::TaskResult;
use std::collections::HashMap;

fn result(task: &str, output: &str, reasoning: Option<&str>) -> TaskResult {
    TaskResult {
        task: task.into(),
        agent: "agent".into(),
        output: output.into(),
        success: true,
        duration_ms: 1,
        token_usage: None,
        reasoning: reasoning.map(str::to_string),
    }
}

#[test]
fn retained_result_budget_rejects_large_output_and_reasoning() {
    let mut results = HashMap::new();
    let mut budget = RetainedResultBudget {
        max_output_bytes: 3,
        max_reasoning_bytes: 2,
        max_total_bytes: 1024,
        total_bytes: 0,
    };

    let output_error = budget
        .insert(&mut results, "one".into(), result("one", "four", None))
        .unwrap_err()
        .to_string();
    assert!(output_error.contains("IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES"));
    assert!(results.is_empty());

    let reasoning_error = budget
        .insert(
            &mut results,
            "one".into(),
            result("one", "ok", Some("long")),
        )
        .unwrap_err()
        .to_string();
    assert!(reasoning_error.contains("IRONCREW_TASK_RESULT_MAX_REASONING_BYTES"));
    assert!(results.is_empty());
}

#[test]
fn retained_result_budget_counts_serialized_bytes_and_replacements() {
    let first = result("one", "line\nwith escaping", None);
    let first_bytes = serialized_result_bytes(&first).unwrap();
    let replacement = result("one", "x", None);
    let replacement_bytes = serialized_result_bytes(&replacement).unwrap();
    let second = result("two", "y", None);
    let second_bytes = serialized_result_bytes(&second).unwrap();
    let mut results = HashMap::new();
    let mut budget = RetainedResultBudget {
        max_output_bytes: 1024,
        max_reasoning_bytes: 1024,
        max_total_bytes: first_bytes - 1,
        total_bytes: 0,
    };

    budget
        .insert(&mut results, "one".into(), first)
        .unwrap_err();
    assert!(results.is_empty());

    budget.max_total_bytes = replacement_bytes + second_bytes;
    budget
        .insert(&mut results, "one".into(), replacement)
        .unwrap();
    budget.insert(&mut results, "two".into(), second).unwrap();
    assert_eq!(budget.total_bytes, replacement_bytes + second_bytes);

    let larger_replacement = result("one", "this no longer fits", None);
    let error = budget
        .insert(&mut results, "one".into(), larger_replacement)
        .unwrap_err()
        .to_string();
    assert!(error.contains("IRONCREW_RUN_RESULTS_MAX_BYTES"));
    assert_eq!(results["one"].output, "x");
}

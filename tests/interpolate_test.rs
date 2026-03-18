use std::collections::HashMap;

use ironcrew::engine::interpolate::interpolate;
use ironcrew::engine::task::TaskResult;

#[test]
fn test_interpolate_output() {
    let mut results = HashMap::new();
    results.insert(
        "research".into(),
        TaskResult {
            task: "research".into(),
            agent: "researcher".into(),
            output: "Rust is fast and safe".into(),
            success: true,
            duration_ms: 1500,
            token_usage: None,
        },
    );

    let template = "Summarize: ${results.research.output}";
    let result = interpolate(template, &results);
    assert_eq!(result, "Summarize: Rust is fast and safe");
}

#[test]
fn test_interpolate_multiple() {
    let mut results = HashMap::new();
    results.insert(
        "step1".into(),
        TaskResult {
            task: "step1".into(),
            agent: "agent1".into(),
            output: "data1".into(),
            success: true,
            duration_ms: 100,
            token_usage: None,
        },
    );
    results.insert(
        "step2".into(),
        TaskResult {
            task: "step2".into(),
            agent: "agent2".into(),
            output: "data2".into(),
            success: true,
            duration_ms: 200,
            token_usage: None,
        },
    );

    let template = "Step1: ${results.step1.output}, Step2: ${results.step2.output}";
    let result = interpolate(template, &results);
    assert_eq!(result, "Step1: data1, Step2: data2");
}

#[test]
fn test_interpolate_success_field() {
    let mut results = HashMap::new();
    results.insert(
        "check".into(),
        TaskResult {
            task: "check".into(),
            agent: "checker".into(),
            output: "done".into(),
            success: true,
            duration_ms: 50,
            token_usage: None,
        },
    );

    let template = "Was successful: ${results.check.success}";
    let result = interpolate(template, &results);
    assert_eq!(result, "Was successful: true");
}

#[test]
fn test_interpolate_missing_task() {
    let results = HashMap::new();
    let template = "Value: ${results.nonexistent.output}";
    let result = interpolate(template, &results);
    assert_eq!(result, "Value: ");
}

#[test]
fn test_interpolate_no_patterns() {
    let results = HashMap::new();
    let template = "Just plain text with no patterns";
    let result = interpolate(template, &results);
    assert_eq!(result, "Just plain text with no patterns");
}

#[test]
fn test_interpolate_env_var() {
    let results = HashMap::new();
    unsafe {
        std::env::set_var("TEST_IRONCREW_INTERP", "hello_world");
    }
    let template = "Env: ${env.TEST_IRONCREW_INTERP}";
    let result = interpolate(template, &results);
    assert_eq!(result, "Env: hello_world");
    unsafe {
        std::env::remove_var("TEST_IRONCREW_INTERP");
    }
}

#[test]
fn test_interpolate_agent_and_duration() {
    let mut results = HashMap::new();
    results.insert(
        "task1".into(),
        TaskResult {
            task: "task1".into(),
            agent: "my_agent".into(),
            output: "output".into(),
            success: true,
            duration_ms: 2500,
            token_usage: None,
        },
    );

    let template = "Agent: ${results.task1.agent}, took ${results.task1.duration_ms}ms";
    let result = interpolate(template, &results);
    assert_eq!(result, "Agent: my_agent, took 2500ms");
}

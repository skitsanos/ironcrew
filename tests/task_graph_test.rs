use std::collections::HashSet;

use ironcrew::engine::task::{topological_phases, validate_dependency_graph, topological_sort, Task};

#[test]
fn test_valid_dependency_graph() {
    let tasks = vec![
        Task { name: "a".into(), description: "Task A".into(), ..Default::default() },
        Task { name: "b".into(), description: "Task B".into(), depends_on: vec!["a".into()], ..Default::default() },
        Task { name: "c".into(), description: "Task C".into(), depends_on: vec!["a".into(), "b".into()], ..Default::default() },
    ];
    assert!(validate_dependency_graph(&tasks).is_ok());
}

#[test]
fn test_missing_dependency() {
    let tasks = vec![
        Task { name: "a".into(), description: "Task A".into(), depends_on: vec!["nonexistent".into()], ..Default::default() },
    ];
    let err = validate_dependency_graph(&tasks).unwrap_err();
    assert!(err.to_string().contains("nonexistent"));
}

#[test]
fn test_circular_dependency() {
    let tasks = vec![
        Task { name: "a".into(), description: "A".into(), depends_on: vec!["b".into()], ..Default::default() },
        Task { name: "b".into(), description: "B".into(), depends_on: vec!["a".into()], ..Default::default() },
    ];
    let err = validate_dependency_graph(&tasks).unwrap_err();
    assert!(err.to_string().contains("Circular dependency"));
}

#[test]
fn test_topological_sort_order() {
    let tasks = vec![
        Task { name: "c".into(), description: "C".into(), depends_on: vec!["b".into()], ..Default::default() },
        Task { name: "a".into(), description: "A".into(), ..Default::default() },
        Task { name: "b".into(), description: "B".into(), depends_on: vec!["a".into()], ..Default::default() },
    ];
    let sorted = topological_sort(&tasks);
    let names: Vec<&str> = sorted.iter().map(|t| t.name.as_str()).collect();
    let pos_a = names.iter().position(|&n| n == "a").unwrap();
    let pos_b = names.iter().position(|&n| n == "b").unwrap();
    let pos_c = names.iter().position(|&n| n == "c").unwrap();
    assert!(pos_a < pos_b);
    assert!(pos_b < pos_c);
}

#[test]
fn test_task_with_retry_fields() {
    let task = Task {
        name: "test".into(),
        description: "test".into(),
        max_retries: Some(3),
        retry_backoff_secs: Some(1.0),
        timeout_secs: Some(60),
        ..Default::default()
    };
    assert_eq!(task.max_retries, Some(3));
    assert_eq!(task.retry_backoff_secs, Some(1.0));
    assert_eq!(task.timeout_secs, Some(60));
}

#[test]
fn test_task_retry_fields_default_to_none() {
    let task = Task {
        name: "test".into(),
        description: "test".into(),
        ..Default::default()
    };
    assert_eq!(task.max_retries, None);
    assert_eq!(task.retry_backoff_secs, None);
    assert_eq!(task.timeout_secs, None);
}

#[test]
fn test_task_with_condition_field() {
    let task = Task {
        name: "conditional".into(),
        description: "test".into(),
        condition: Some("results.step1.success == true".into()),
        ..Default::default()
    };
    assert_eq!(
        task.condition,
        Some("results.step1.success == true".into())
    );
}

#[test]
fn test_task_with_on_error_field() {
    let task = Task {
        name: "risky".into(),
        description: "test".into(),
        on_error: Some("error_handler".into()),
        ..Default::default()
    };
    assert_eq!(task.on_error, Some("error_handler".into()));
}

#[test]
fn test_topological_phases_diamond() {
    let tasks = vec![
        Task { name: "a".into(), description: "A".into(), ..Default::default() },
        Task { name: "b".into(), description: "B".into(), ..Default::default() },
        Task { name: "c".into(), description: "C".into(), depends_on: vec!["a".into(), "b".into()], ..Default::default() },
        Task { name: "d".into(), description: "D".into(), depends_on: vec!["c".into()], ..Default::default() },
    ];

    let phases = topological_phases(&tasks);
    assert_eq!(phases.len(), 3); // phase 0: [a, b], phase 1: [c], phase 2: [d]

    // Phase 0 should have a and b (no deps)
    let phase0_names: HashSet<&str> = phases[0].iter().map(|t| t.name.as_str()).collect();
    assert!(phase0_names.contains("a"));
    assert!(phase0_names.contains("b"));
    assert_eq!(phase0_names.len(), 2);

    // Phase 1 should have c
    assert_eq!(phases[1].len(), 1);
    assert_eq!(phases[1][0].name, "c");

    // Phase 2 should have d
    assert_eq!(phases[2].len(), 1);
    assert_eq!(phases[2][0].name, "d");
}

#[test]
fn test_topological_phases_all_independent() {
    let tasks = vec![
        Task { name: "a".into(), description: "A".into(), ..Default::default() },
        Task { name: "b".into(), description: "B".into(), ..Default::default() },
        Task { name: "c".into(), description: "C".into(), ..Default::default() },
    ];

    let phases = topological_phases(&tasks);
    assert_eq!(phases.len(), 1); // all in one phase
    assert_eq!(phases[0].len(), 3);
}

#[test]
fn test_topological_phases_linear_chain() {
    let tasks = vec![
        Task { name: "a".into(), description: "A".into(), ..Default::default() },
        Task { name: "b".into(), description: "B".into(), depends_on: vec!["a".into()], ..Default::default() },
        Task { name: "c".into(), description: "C".into(), depends_on: vec!["b".into()], ..Default::default() },
    ];

    let phases = topological_phases(&tasks);
    assert_eq!(phases.len(), 3); // each task in its own phase
    assert_eq!(phases[0].len(), 1);
    assert_eq!(phases[0][0].name, "a");
    assert_eq!(phases[1].len(), 1);
    assert_eq!(phases[1][0].name, "b");
    assert_eq!(phases[2].len(), 1);
    assert_eq!(phases[2][0].name, "c");
}

#[test]
fn test_topological_phases_empty() {
    let tasks: Vec<Task> = vec![];
    let phases = topological_phases(&tasks);
    assert!(phases.is_empty());
}

#[test]
fn test_task_with_foreach_fields() {
    let task = Task {
        name: "process".into(),
        description: "Process ${item}".into(),
        foreach_source: Some("items_list".into()),
        foreach_as: Some("item".into()),
        ..Default::default()
    };
    assert_eq!(task.foreach_source, Some("items_list".into()));
    assert_eq!(task.foreach_as, Some("item".into()));
}

#[test]
fn test_task_foreach_fields_default_to_none() {
    let task = Task {
        name: "test".into(),
        description: "test".into(),
        ..Default::default()
    };
    assert_eq!(task.foreach_source, None);
    assert_eq!(task.foreach_as, None);
}

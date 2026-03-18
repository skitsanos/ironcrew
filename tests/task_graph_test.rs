use ironcrew::engine::task::{validate_dependency_graph, topological_sort, Task};

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

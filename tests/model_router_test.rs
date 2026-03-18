use ironcrew::engine::model_router::ModelRouter;
use ironcrew::engine::task::Task;

#[test]
fn test_model_router_resolve() {
    let mut router = ModelRouter::new();
    router.set("collaboration", "gpt-4o".into());
    router.set("task_execution", "gpt-4o-mini".into());

    assert_eq!(router.resolve("collaboration", "fallback"), "gpt-4o");
    assert_eq!(router.resolve("task_execution", "fallback"), "gpt-4o-mini");
    assert_eq!(router.resolve("unknown", "fallback"), "fallback");
}

#[test]
fn test_model_router_with_default() {
    let mut router = ModelRouter::new();
    router.set_default("gpt-4o".into());

    assert_eq!(router.resolve("anything", "fallback"), "gpt-4o");
}

#[test]
fn test_model_router_empty() {
    let router = ModelRouter::new();
    assert!(!router.is_configured());
    assert_eq!(
        router.resolve("task_execution", "gpt-4o-mini"),
        "gpt-4o-mini"
    );
}

#[test]
fn test_task_model_field() {
    let task = Task {
        name: "test".into(),
        description: "test".into(),
        model: Some("gpt-4o".into()),
        ..Default::default()
    };
    assert_eq!(task.model, Some("gpt-4o".into()));
}

#[test]
fn test_model_router_route_overrides_default() {
    let mut router = ModelRouter::new();
    router.set_default("gpt-4o-mini".into());
    router.set("collaboration", "gpt-4o".into());

    // Specific route takes precedence over default
    assert_eq!(router.resolve("collaboration", "fallback"), "gpt-4o");
    // Unknown purpose falls back to default
    assert_eq!(router.resolve("unknown", "fallback"), "gpt-4o-mini");
}

#[test]
fn test_model_router_is_configured() {
    let mut router = ModelRouter::new();
    assert!(!router.is_configured());

    router.set("task_execution", "gpt-4o".into());
    assert!(router.is_configured());
}

#[test]
fn test_model_router_is_configured_with_default_only() {
    let mut router = ModelRouter::new();
    assert!(!router.is_configured());

    router.set_default("gpt-4o".into());
    assert!(router.is_configured());
}

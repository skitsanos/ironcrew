use super::super::ToolRegistry;
use super::support::{DefinedTool, registry};

#[test]
fn resolved_schema_availability_and_approval_are_bound() {
    let names = vec!["lookup".to_string()];
    let baseline = registry("first")
        .conversation_execution_fingerprint(&names)
        .unwrap();
    assert_ne!(
        baseline,
        registry("changed")
            .conversation_execution_fingerprint(&names)
            .unwrap()
    );
    assert!(
        ToolRegistry::new()
            .conversation_execution_fingerprint(&names)
            .unwrap_err()
            .to_string()
            .contains("not registered")
    );

    let mut gated = registry("first");
    gated.set_approval_policy(crate::tools::approval::ApprovalPolicy::from_rules(&[
        "lookup".into(),
    ]));
    assert_ne!(
        baseline,
        gated.conversation_execution_fingerprint(&names).unwrap()
    );

    let approval_fingerprint = |timeout_secs, args_max_chars| {
        let mut registry = registry("first");
        registry.set_approval_policy(
            crate::tools::approval::ApprovalPolicy::with_limits_for_test(
                &["lookup".into()],
                timeout_secs,
                args_max_chars,
            ),
        );
        registry.conversation_execution_fingerprint(&names).unwrap()
    };
    assert_ne!(approval_fingerprint(30, 600), approval_fingerprint(31, 600));
    assert_ne!(approval_fingerprint(30, 600), approval_fingerprint(30, 601));
}

#[test]
fn reachable_dependencies_are_bound_once() {
    let mut original = registry("dependency");
    original.register(Box::new(DefinedTool {
        name: "delegate".into(),
        description: "delegate".into(),
        dependencies: vec!["lookup".into(), "lookup".into()],
    }));
    let with_dependency = original
        .conversation_execution_fingerprint(&["delegate".into()])
        .unwrap();

    let mut changed = registry("changed dependency");
    changed.register(Box::new(DefinedTool {
        name: "delegate".into(),
        description: "delegate".into(),
        dependencies: vec!["lookup".into()],
    }));
    assert_ne!(
        with_dependency,
        changed
            .conversation_execution_fingerprint(&["delegate".into()])
            .unwrap()
    );
}

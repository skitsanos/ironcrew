use super::prometheus;
use super::state::Metrics;
use super::{HookFailureStage, HookKind};

fn render(metrics: &Metrics) -> String {
    let mut body = String::new();
    prometheus::append(&mut body, metrics);
    body
}

#[test]
fn hook_labels_are_a_closed_vocabulary() {
    assert_eq!(
        HookKind::ALL
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        ["before_task", "after_task"]
    );
    assert_eq!(
        HookFailureStage::ALL
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        [
            "vm_initialization",
            "execution_start",
            "environment",
            "load",
            "run",
            "return_value"
        ]
    );
}

#[test]
fn hook_failures_are_rendered_with_fixed_hook_and_stage_labels() {
    let metrics = Metrics::default();
    metrics.record_hook_failure(HookKind::BeforeTask, HookFailureStage::Load);
    metrics.record_hook_failure(HookKind::BeforeTask, HookFailureStage::Load);
    let body = render(&metrics);

    assert!(body.contains("ironcrew_hook_failures_total{hook=\"before_task\",stage=\"load\"} 2\n"));
    assert_eq!(
        body.lines()
            .filter(|line| line.starts_with("ironcrew_hook_failures_total{"))
            .count(),
        HookKind::COUNT * HookFailureStage::COUNT
    );
}

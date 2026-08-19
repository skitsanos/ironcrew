//! Route shapes assigned to bounded read-side admission.

pub(super) fn is_flow_inspection(path: &str) -> bool {
    let mut segments = path.trim_matches('/').split('/');
    matches!(
        (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ),
        (Some("flows"), Some(flow), Some("validate" | "agents"), None) if !flow.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use super::is_flow_inspection;

    #[test]
    fn recognizes_only_exact_inspection_routes() {
        assert!(is_flow_inspection("/flows/a/validate"));
        assert!(is_flow_inspection("/flows/a/agents"));
        assert!(!is_flow_inspection("/flows/a/validate/extra"));
        assert!(!is_flow_inspection("/other/a/agents"));
    }
}

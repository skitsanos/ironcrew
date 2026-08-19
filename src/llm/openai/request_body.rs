use serde_json::{Value, json};

use crate::llm::provider::ToolSchema;

/// Adds the Chat Completions output-token limit using the field supported by
/// the selected model.
///
/// OpenAI's GPT-5 family rejects the legacy `max_tokens` field. Other model
/// IDs retain that field because this provider also targets third-party
/// OpenAI-compatible endpoints whose request contracts must not change
/// implicitly.
pub(super) fn insert_completion_token_limit(
    body: &mut Value,
    model: &str,
    max_tokens: Option<u32>,
) {
    let Some(max_tokens) = max_tokens else {
        return;
    };

    let field = if model.starts_with("gpt-5") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    body[field] = json!(max_tokens);
}

/// Keeps Luna's Chat Completions tool calls on its supported execution path.
/// Luna defaults to reasoning when the field is omitted, but the API rejects
/// function tools unless reasoning effort is explicitly disabled.
pub(super) fn insert_tool_reasoning_compatibility(body: &mut Value, model: &str, has_tools: bool) {
    let is_luna = model == "gpt-5.6-luna" || model.starts_with("gpt-5.6-luna-");
    if has_tools && is_luna {
        body["reasoning_effort"] = json!("none");
    }
}

pub(super) fn insert_tools(body: &mut Value, model: &str, tools: Option<&[ToolSchema]>) {
    if let Some(schemas) = tools {
        body["tools"] = json!(
            schemas
                .iter()
                .map(|tool| json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                }))
                .collect::<Vec<_>>()
        );
    }
    insert_tool_reasoning_compatibility(
        body,
        model,
        tools.is_some_and(|schemas| !schemas.is_empty()),
    );
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use serde_json::json;

    use super::{insert_completion_token_limit, insert_tool_reasoning_compatibility};
    use crate::llm::execution_policy::ProviderExecutionPolicy;
    use crate::llm::openai::OpenAiProvider;
    use crate::llm::provider::LlmProvider;
    use crate::utils::error::IronCrewError;

    fn policy(request_bytes: usize) -> ProviderExecutionPolicy {
        ProviderExecutionPolicy::from_values(
            None,
            [
                request_bytes,
                16 * 1024 * 1024,
                256 * 1024,
                16 * 1024 * 1024,
                32 * 1024 * 1024,
            ],
            [10, 900],
        )
    }

    #[test]
    fn luna_uses_max_completion_tokens() {
        let mut body = json!({});

        insert_completion_token_limit(&mut body, "gpt-5.6-luna", Some(128));

        assert_eq!(body["max_completion_tokens"], 128);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn other_gpt_5_models_use_max_completion_tokens() {
        for model in ["gpt-5.6-terra", "gpt-5.4"] {
            let mut body = json!({});

            insert_completion_token_limit(&mut body, model, Some(128));

            assert_eq!(body["max_completion_tokens"], 128, "model: {model}");
            assert!(body.get("max_tokens").is_none(), "model: {model}");
        }
    }

    #[test]
    fn other_models_keep_max_tokens() {
        let mut body = json!({});

        insert_completion_token_limit(&mut body, "gpt-4.1-mini", Some(128));

        assert_eq!(body["max_tokens"], 128);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn omitted_limit_adds_no_token_field() {
        let mut body = json!({});

        insert_completion_token_limit(&mut body, "gpt-5.6-luna", None);

        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn luna_function_tools_explicitly_disable_reasoning() {
        for model in ["gpt-5.6-luna", "gpt-5.6-luna-2026-08-01"] {
            let mut body = json!({});

            insert_tool_reasoning_compatibility(&mut body, model, true);

            assert_eq!(body["reasoning_effort"], "none", "model: {model}");
        }
    }

    #[test]
    fn tool_free_luna_and_other_models_keep_provider_defaults() {
        for (model, has_tools) in [
            ("gpt-5.6-luna", false),
            ("gpt-5.6-lunatic", true),
            ("gpt-5.6-terra", true),
        ] {
            let mut body = json!({});

            insert_tool_reasoning_compatibility(&mut body, model, has_tools);

            assert!(body.get("reasoning_effort").is_none(), "model: {model}");
        }
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_before_network_validation_or_send() {
        let body = json!({"sentinel_secret": "must-not-appear"});
        let actual = serde_json::to_vec(&body).unwrap().len();
        let mut provider = OpenAiProvider::new(
            "not-a-real-key".into(),
            Some("https://127.0.0.1:9/v1".into()),
        );
        provider.execution_policy = policy(actual - 1);

        let error = provider.send_request(body).await.unwrap_err();
        assert!(matches!(
            error,
            IronCrewError::ProviderRequestTooLarge {
                provider: "OpenAI",
                actual: observed,
                limit
            } if observed == actual && limit == actual - 1
        ));
        assert!(!error.to_string().contains("must-not-appear"));
    }

    #[test]
    fn request_cap_drift_changes_provider_execution_fingerprint() {
        let mut first = OpenAiProvider::new("one".into(), None);
        let mut second = OpenAiProvider::new("two".into(), None);
        first.execution_policy = policy(15_000);
        second.execution_policy = policy(15_001);

        assert_ne!(
            first.execution_fingerprint().unwrap(),
            second.execution_fingerprint().unwrap()
        );
    }

    #[test]
    fn evaluator_request_cap_is_captured_from_an_isolated_environment() {
        if std::env::var_os("IRONCREW_PROVIDER_REQUEST_CAP_CHILD").is_some() {
            return;
        }
        let status = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "llm::openai::request_body::tests::evaluator_request_cap_rejects_before_network_child",
                "--nocapture",
            ])
            .env("IRONCREW_PROVIDER_REQUEST_CAP_CHILD", "1")
            .env("IRONCREW_PROVIDER_MAX_REQUEST_BYTES", "18000")
            .env_remove("IRONCREW_ALLOW_PRIVATE_IPS")
            .status()
            .expect("run isolated provider request-cap child");
        assert!(
            status.success(),
            "isolated request-cap child failed: {status}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluator_request_cap_rejects_before_network_child() {
        if std::env::var_os("IRONCREW_PROVIDER_REQUEST_CAP_CHILD").is_none() {
            return;
        }
        let provider = OpenAiProvider::new(
            "not-a-real-key".into(),
            Some("https://127.0.0.1:9/v1".into()),
        );
        let error = provider
            .send_request(json!({"payload": "x".repeat(18_000)}))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            IronCrewError::ProviderRequestTooLarge {
                provider: "OpenAI",
                actual,
                limit: 18_000,
            } if actual > 18_000
        ));
    }
}

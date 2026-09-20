use super::*;
use crate::engine::agent::ResponseFormat;

impl OpenAiProvider {
    pub(super) fn build_body(
        &self,
        request: &ChatRequest,
        tools: Option<&[ToolSchema]>,
    ) -> Result<Value> {
        let options =
            self.resolve_options(request, tools.is_some_and(|tools| !tools.is_empty()))?;
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| {
                let mut msg = json!({"role": m.role});
                // When images are attached, serialize content as an array of
                // content parts (text + image_url blocks). This is the OpenAI
                // vision format, also used by Gemini and other OpenAI-compatible
                // endpoints.
                if let Some(ref images) = m.images {
                    if !images.is_empty() {
                        let mut parts: Vec<serde_json::Value> = Vec::new();
                        if let Some(ref text) = m.content {
                            parts.push(json!({"type": "text", "text": text}));
                        }
                        for img in images {
                            let data_uri = format!("data:{};base64,{}", img.mime_type, img.data);
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": data_uri }
                            }));
                        }
                        msg["content"] = json!(parts);
                    } else if let Some(ref content) = m.content {
                        msg["content"] = json!(content);
                    }
                } else if let Some(ref content) = m.content {
                    msg["content"] = json!(content);
                }
                if let Some(ref tool_call_id) = m.tool_call_id {
                    msg["tool_call_id"] = json!(tool_call_id);
                }
                if let Some(ref tool_calls) = m.tool_calls {
                    msg["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_default();
                }
                msg
            })
            .collect();

        let mut body = json!({
            "model": request.model,
            "messages": messages,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body[options.token_field] = json!(max);
        }

        if let Some(ref fmt) = request.response_format {
            match fmt {
                ResponseFormat::Text => {
                    body["response_format"] = json!({"type": "text"});
                }
                ResponseFormat::JsonObject => {
                    body["response_format"] = json!({"type": "json_object"});
                }
                ResponseFormat::JsonSchema { name, schema } => {
                    body["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": name,
                            "schema": schema,
                            "strict": true,
                        }
                    });
                }
            }
        }

        insert_tools(&mut body, tools);
        if let Some(effort) = options.effort {
            body["reasoning_effort"] = json!(effort);
        }

        if let Some(ref key) = request.prompt_cache_key {
            body["prompt_cache_key"] = json!(key);
        }
        if let Some(ref retention) = request.prompt_cache_retention {
            body["prompt_cache_retention"] = json!(retention);
        }

        Ok(body)
    }

    pub(super) fn resolve_options<'a>(
        &self,
        request: &'a ChatRequest,
        has_tools: bool,
    ) -> Result<super::super::capabilities::Resolved<'a>> {
        super::super::capabilities::openai(
            super::super::capabilities::Transport::ChatCompletions,
            &self.base_url,
            request,
            has_tools,
            None,
        )
    }
}

fn insert_tools(body: &mut Value, tools: Option<&[ToolSchema]>) {
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
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use serde_json::json;

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

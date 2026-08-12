//! Secret-free identity for the effective provider used by a conversation.

use serde_json::Value;

use super::conversation_definition::FramedDigest;
use crate::utils::error::{IronCrewError, Result};

const PROVIDER_DOMAIN: &[u8] = b"ironcrew:conversation-provider:v1";
const TOOLS_DOMAIN: &[u8] = b"ironcrew:conversation-tools:v1";
const MAX_PROVIDER_OPTIONS_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_DEFINITION_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVIDER_ENDPOINT_BYTES: usize = 4_096;

/// Validate the non-secret endpoint portion shared by provider construction
/// and durable identity. Authentication belongs in the separate API-key
/// field; query strings, fragments, and userinfo are therefore rejected.
pub fn validate_provider_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.is_empty() || endpoint.len() > MAX_PROVIDER_ENDPOINT_BYTES {
        return Err(IronCrewError::Validation(
            "provider endpoint must be non-empty and at most 4096 bytes".into(),
        ));
    }
    let parsed = reqwest::Url::parse(endpoint).map_err(|_| {
        IronCrewError::Validation("provider endpoint must be a valid HTTP(S) URL".into())
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(IronCrewError::Validation(
            "provider endpoint must use http or https".into(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(IronCrewError::Validation(
            "provider endpoint must not contain embedded credentials".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(IronCrewError::Validation(
            "provider endpoint must not contain a query string or fragment".into(),
        ));
    }
    Ok(())
}

/// Hash the resolved endpoint and every provider-specific request option.
///
/// `options` must contain no credentials. Callers retain API keys separately;
/// changing a credential must not make an otherwise compatible transcript
/// impossible to resume.
pub fn provider_execution_fingerprint(
    provider_kind: &str,
    effective_base_url: &str,
    options: &Value,
) -> Result<String> {
    if provider_kind.trim().is_empty() || effective_base_url.trim().is_empty() {
        return Err(IronCrewError::Validation(
            "conversation provider identity requires a provider and effective endpoint".into(),
        ));
    }
    validate_provider_endpoint(effective_base_url)?;
    let serialized = serde_json::to_vec(options).map_err(|error| {
        IronCrewError::Validation(format!(
            "failed to serialize conversation provider options: {error}"
        ))
    })?;
    if serialized.len() > MAX_PROVIDER_OPTIONS_BYTES {
        return Err(IronCrewError::Validation(format!(
            "conversation provider options exceed {MAX_PROVIDER_OPTIONS_BYTES} bytes"
        )));
    }

    let mut digest = FramedDigest::new(PROVIDER_DOMAIN);
    digest.field(b"provider_kind", provider_kind.as_bytes());
    digest.field(b"effective_base_url", effective_base_url.as_bytes());
    digest.field(b"options_encoding", b"canonical-json-v1");
    digest.json(options);
    Ok(digest.finish())
}

/// Hash the ordered, resolved tool graph used to construct provider schemas
/// and dispatch calls for a durable conversation.
pub fn resolved_tools_fingerprint(definition: &Value) -> Result<String> {
    let serialized = serde_json::to_vec(definition).map_err(|error| {
        IronCrewError::Validation(format!(
            "failed to serialize resolved conversation tools: {error}"
        ))
    })?;
    if serialized.len() > MAX_TOOL_DEFINITION_BYTES {
        return Err(IronCrewError::Validation(format!(
            "resolved conversation tools exceed {MAX_TOOL_DEFINITION_BYTES} bytes"
        )));
    }
    let mut digest = FramedDigest::new(TOOLS_DOMAIN);
    digest.field(b"definition_encoding", b"canonical-json-v1");
    digest.json(definition);
    Ok(digest.finish())
}

/// Hash an operator-supplied, non-secret execution identity without retaining
/// its raw value in provider/tool definitions or durable records.
pub fn explicit_execution_identity_fingerprint(
    domain: &str,
    label: &str,
    identity: &str,
) -> Result<String> {
    if domain.trim().is_empty()
        || label.trim().is_empty()
        || identity.trim().is_empty()
        || identity.len() > 4_096
        || identity.chars().any(char::is_control)
    {
        return Err(IronCrewError::Validation(
            "execution identity must be non-empty, control-free, and at most 4096 bytes".into(),
        ));
    }
    let mut digest = FramedDigest::new(b"ironcrew:explicit-execution-identity:v1");
    digest.field(b"domain", domain.as_bytes());
    digest.field(b"label", label.as_bytes());
    digest.field(b"identity", identity.as_bytes());
    Ok(digest.finish())
}

/// Stable placeholder used only by id-less, in-memory conversations whose
/// embedded test/custom provider does not expose a durable identity.
pub fn unidentified_ephemeral_provider_fingerprint() -> String {
    "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::llm::anthropic::{AnthropicConfig, AnthropicProvider, ServerTool};
    use crate::llm::metrics::observe_provider;
    use crate::llm::openai::OpenAiProvider;
    use crate::llm::openai_responses::{
        OpenAiResponsesProvider, ResponsesConfig, ServerTool as ResponsesServerTool,
    };
    use crate::llm::provider::LlmProvider;

    #[test]
    fn endpoint_and_options_are_bound_in_canonical_object_order() {
        let left = provider_execution_fingerprint(
            "openai-responses",
            "https://example.test",
            &json!({"reasoning": "high", "tools": ["web"]}),
        )
        .unwrap();
        let reordered = provider_execution_fingerprint(
            "openai-responses",
            "https://example.test",
            &json!({"tools": ["web"], "reasoning": "high"}),
        )
        .unwrap();
        assert_eq!(left, reordered);
        assert_ne!(
            left,
            provider_execution_fingerprint(
                "openai-responses",
                "https://other.test",
                &json!({"reasoning": "high", "tools": ["web"]}),
            )
            .unwrap()
        );
        assert_ne!(
            left,
            provider_execution_fingerprint(
                "openai-responses",
                "https://example.test",
                &json!({"reasoning": "low", "tools": ["web"]}),
            )
            .unwrap()
        );
    }

    #[test]
    fn secret_bearing_endpoint_components_are_rejected_without_echo() {
        for endpoint in [
            "https://user:sentinel@example.test/v1",
            "https://example.test/v1?api_key=sentinel",
            "https://example.test/v1#sentinel",
        ] {
            let error = provider_execution_fingerprint("openai", endpoint, &json!({}))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("sentinel"));
        }
    }

    #[test]
    fn concrete_providers_bind_semantics_but_not_credentials() {
        let default_one = OpenAiProvider::new("secret-one".into(), None);
        let default_two = OpenAiProvider::new(
            "secret-two".into(),
            Some("https://api.openai.com/v1".into()),
        );
        assert_eq!(
            default_one.execution_fingerprint().unwrap(),
            default_two.execution_fingerprint().unwrap()
        );
        assert_ne!(
            default_one.execution_fingerprint().unwrap(),
            OpenAiProvider::new("secret-one".into(), Some("https://example.test/v1".into()))
                .execution_fingerprint()
                .unwrap()
        );

        let anthropic = AnthropicProvider::new(
            "secret".into(),
            None,
            AnthropicConfig {
                thinking_budget: Some(2_048),
                server_tools: vec![ServerTool::WebSearch { max_uses: Some(3) }],
            },
        );
        let anthropic_changed = AnthropicProvider::new(
            "other-secret".into(),
            None,
            AnthropicConfig {
                thinking_budget: Some(4_096),
                server_tools: vec![ServerTool::WebSearch { max_uses: Some(3) }],
            },
        );
        assert_ne!(
            anthropic.execution_fingerprint().unwrap(),
            anthropic_changed.execution_fingerprint().unwrap()
        );

        let responses = OpenAiResponsesProvider::new(
            "secret".into(),
            None,
            ResponsesConfig {
                reasoning_effort: Some("high".into()),
                reasoning_summary: Some("concise".into()),
                server_tools: vec![ResponsesServerTool::FileSearch {
                    vector_store_ids: vec!["vs-one".into()],
                    max_num_results: Some(5),
                }],
            },
        );
        let responses_changed = OpenAiResponsesProvider::new(
            "secret".into(),
            None,
            ResponsesConfig {
                reasoning_effort: Some("low".into()),
                reasoning_summary: Some("concise".into()),
                server_tools: vec![ResponsesServerTool::FileSearch {
                    vector_store_ids: vec!["vs-one".into()],
                    max_num_results: Some(5),
                }],
            },
        );
        assert_ne!(
            responses.execution_fingerprint().unwrap(),
            responses_changed.execution_fingerprint().unwrap()
        );
    }

    #[test]
    fn metrics_wrapper_preserves_provider_identity() {
        let inner: Arc<dyn LlmProvider> = Arc::new(OpenAiProvider::new("secret".into(), None));
        let expected = inner.execution_fingerprint().unwrap();
        assert_eq!(
            observe_provider(inner).execution_fingerprint().unwrap(),
            expected
        );
    }
}

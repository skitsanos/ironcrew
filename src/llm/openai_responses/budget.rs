//! Exact preflight input count plus an explicit all-output token bound.
use super::*;
use crate::usage::budget::{BudgetError, DEFAULT_BUDGET_OUTPUT_TOKENS, TokenBudget};

impl OpenAiResponsesProvider {
    pub(super) fn budget_supported(&self) -> bool {
        let native = self.base_url.trim_end_matches('/') == "https://api.openai.com";
        #[cfg(test)]
        let native = native || self.base_url.starts_with("http://127.0.0.1:");
        native && self.config.server_tools.is_empty()
    }

    pub(super) async fn prepare_dispatch(
        &self,
        mut body: Value,
        tracker: Option<&UsageTracker>,
    ) -> Result<(Vec<u8>, ProviderAttempt)> {
        let budget = match tracker {
            Some(tracker) => tracker.budget().clone(),
            None => TokenBudget::from_environment()?,
        };
        budget.check()?;
        if budget.enabled() && (tracker.is_none() || !self.budget_supported()) {
            return Err(budget.block(BudgetError::Unsupported).into());
        }
        let url = format!("{}/v1/responses", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;
        let bound = if budget.enabled() {
            let output = match body.get("max_output_tokens") {
                Some(value) => value
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| budget.block(BudgetError::Unsupported))?,
                None => DEFAULT_BUDGET_OUTPUT_TOKENS,
            };
            body["max_output_tokens"] = json!(output);
            // Validate final generation bytes before issuing even the count request.
            self.prepare_request(&body)?;
            let input = self.count_input(&body).await.map_err(|error| {
                let reason =
                    if matches!(error, IronCrewError::TokenBudget(BudgetError::Unsupported)) {
                        BudgetError::Unsupported
                    } else {
                        BudgetError::CountingFailed
                    };
                IronCrewError::TokenBudget(budget.block(reason))
            })?;
            Some((input, output))
        } else {
            None
        };
        let bytes = self.prepare_request(&body)?;
        if let Some(limiter) = &self.rate_limit {
            limiter.wait().await;
        }
        let reservation = bound
            .map(|(input, output)| budget.reserve(input, output))
            .transpose()?;
        let accounting =
            ProviderAttempt::start_reserved(tracker, ProviderUsage::OpenAiResponses, reservation)?;
        Ok((bytes, accounting))
    }

    async fn count_input(&self, body: &Value) -> Result<u64> {
        // New generation fields must be reviewed explicitly, never silently
        // omitted from the count payload. The known exclusions do not add input.
        let mut count = serde_json::Map::new();
        for (key, value) in body.as_object().expect("Responses request object") {
            match key.as_str() {
                "model"
                | "input"
                | "instructions"
                | "reasoning"
                | "text"
                | "tools"
                | "tool_choice"
                | "parallel_tool_calls" => {
                    count.insert(key.clone(), value.clone());
                }
                "store" | "stream" | "include" | "temperature" | "max_output_tokens" => {}
                _ => return Err(BudgetError::Unsupported.into()),
            }
        }
        // Refuse remotely mutable or hidden input references. IronCrew sends
        // full history and inline images; it never needs provider-side history.
        for item in body["input"].as_array().into_iter().flatten() {
            if !matches!(
                item["type"].as_str(),
                Some("message" | "function_call" | "function_call_output" | "reasoning")
            ) {
                return Err(BudgetError::Unsupported.into());
            }
        }
        let bytes = self.prepare_request(&Value::Object(count))?;
        if let Some(limiter) = &self.rate_limit {
            limiter.wait().await;
        }
        let response = self
            .client
            .post(format!("{}/v1/responses/input_tokens", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .timeout(
                self.execution_policy
                    .request_timeout()
                    .min(std::time::Duration::from_secs(15)),
            )
            .body(bytes)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(BudgetError::CountingFailed.into());
        }
        let bytes = crate::utils::http::read_response_bytes(response, 4096, "input token count")
            .await
            .map_err(|_| BudgetError::CountingFailed)?;
        let result: Value =
            serde_json::from_slice(&bytes).map_err(|_| BudgetError::CountingFailed)?;
        if result["object"] != "response.input_tokens" {
            return Err(BudgetError::CountingFailed.into());
        }
        result["input_tokens"]
            .as_u64()
            .ok_or_else(|| BudgetError::CountingFailed.into())
    }
}

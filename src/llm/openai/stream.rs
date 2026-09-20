use super::*;

impl OpenAiProvider {
    pub(super) async fn send_request_stream(
        &self,
        mut body: Value,
        usage_tracker: Option<&UsageTracker>,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/chat/completions", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let mut accounting = ProviderAttempt::start(usage_tracker, ProviderUsage::OpenAiChat)?;
        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        if !resp.status().is_success() {
            return Err(
                read_error_response(resp, self.execution_policy, "OpenAI error response")
                    .await?
                    .into_accounted_error(&mut accounting),
            );
        }

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let output_limit = self.execution_policy.output_bytes();
        let mut stored_output_bytes = 0_usize;
        // Track tool call assembly (streaming sends deltas)
        let mut tool_call_buffers = stream_tools::StreamToolCalls::default();

        let mut lines = ProviderSseLines::new(resp, self.execution_policy, "OpenAI stream");
        // Track terminal delivery: a mid-stream `{"error": …}` chunk or a stream
        // that ends before `data: [DONE]` must fail rather than return whatever
        // partial content accumulated.
        let mut stream_error: Option<String> = None;
        let mut saw_done = false;

        while let Some(raw_line) = lines.next_line().await? {
            let line = raw_line.trim();

            if line.is_empty() {
                continue;
            }

            if line == "data: [DONE]" {
                saw_done = true;
                let _ = tx.send(StreamChunk::Done).await;
                continue;
            }

            if let Some(data) = sse_field(line, "data")
                && let Ok(parsed) = serde_json::from_str::<Value>(data)
            {
                // The usage-only chunk precedes [DONE]; retain it even if
                // content processing or the connection subsequently fails.
                let final_usage = parsed
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                    && parsed.get("usage").is_some_and(Value::is_object);
                accounting.observe(parsed.get("usage"), final_usage);
                // Some OpenAI-compatible servers report failures as an inline
                // `{"error": {...}}` data event mid-stream.
                if let Some(err) = parsed.get("error").filter(|e| !e.is_null()) {
                    let msg = err["message"]
                        .as_str()
                        .or_else(|| err.as_str())
                        .unwrap_or("stream error");
                    stream_error = Some(msg.to_string());
                    break;
                }

                let delta = &parsed["choices"][0]["delta"];

                // Text content delta
                if let Some(content) = delta["content"].as_str() {
                    crate::utils::http::bounded_push_str(
                        &mut full_content,
                        content,
                        &mut stored_output_bytes,
                        output_limit,
                        "OpenAI accumulated output",
                    )
                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                    let _ = tx.send(StreamChunk::Text(content.to_string())).await;
                }

                // Reasoning delta (DeepSeek, Kimi, Moonshot use reasoning_content)
                if let Some(reasoning) = delta["reasoning_content"]
                    .as_str()
                    .or_else(|| delta["reasoning"].as_str())
                {
                    crate::utils::http::bounded_push_str(
                        &mut full_reasoning,
                        reasoning,
                        &mut stored_output_bytes,
                        output_limit,
                        "OpenAI accumulated output",
                    )
                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                    let _ = tx.send(StreamChunk::Thinking(reasoning.to_string())).await;
                }

                // Tool calls delta
                if let Some(tc_deltas) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tc_deltas {
                        let updates = tool_call_buffers.apply_delta(
                            tc,
                            &mut stored_output_bytes,
                            output_limit,
                        )?;
                        if let Some((id, name)) = updates.start {
                            let _ = tx.try_send(StreamChunk::ToolCallStart { id, name });
                        }
                        if let Some((id, arguments_delta)) = updates.arguments {
                            let _ = tx.try_send(StreamChunk::ToolCallDelta {
                                id,
                                arguments_delta,
                            });
                        }
                    }
                }
            }
        }

        if let Some(err) = stream_error {
            return Err(IronCrewError::Provider(format!(
                "OpenAI stream error — {err}"
            )));
        }

        // Assemble tool calls from buffers
        let tool_calls = tool_call_buffers.finish()?;

        // A stream that ended before `[DONE]` *and* produced nothing is a
        // truncated/dropped connection — fail with a clear message instead of
        // the misleading "Empty response from LLM" downstream. We don't fail a
        // content-bearing stream on a missing `[DONE]`, since some
        // OpenAI-compatible providers omit that terminal marker.
        if !saw_done && full_content.is_empty() && tool_calls.is_empty() {
            return Err(IronCrewError::Provider(
                "OpenAI stream ended before [DONE] with no content (truncated response)".into(),
            ));
        }

        let content = if full_content.is_empty() {
            None
        } else {
            Some(full_content)
        };

        let reasoning = if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        };

        accounting.finish()?;
        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage: None,
            raw_blocks: None,
        })
    }
}

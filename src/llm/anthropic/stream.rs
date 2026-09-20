use super::*;
mod output;

impl AnthropicProvider {
    /// Send a streaming request to the Anthropic Messages API.
    pub(super) async fn send_request_stream(
        &self,
        mut body: Value,
        structured_output_tool: Option<&str>,
        usage_tracker: Option<&UsageTracker>,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        body["stream"] = json!(true);
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/messages", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let mut accounting = ProviderAttempt::start(usage_tracker, ProviderUsage::Anthropic)?;
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(request_body)
            .send()
            .await
            .map_err(IronCrewError::Http)?;

        if !resp.status().is_success() {
            return Err(read_error_response(
                resp,
                self.execution_policy,
                "Anthropic error response",
            )
            .await?
            .into_accounted_error(&mut accounting));
        }

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let output_limit = self.execution_policy.output_bytes();
        let mut stored_output_bytes = 0_usize;
        let mut block_states: BTreeMap<usize, BlockState> = BTreeMap::new();
        let mut input_tokens: u32 = 0;
        let mut output_tokens: u32 = 0;
        let mut cached_tokens: u32 = 0;

        let mut lines = ProviderSseLines::new(resp, self.execution_policy, "Anthropic stream");
        let mut current_event_type = String::new();
        // Track terminal delivery so a mid-stream `error` event (e.g.
        // `overloaded_error`) or a connection dropped before `message_stop`
        // surfaces as an error instead of a silently-truncated success.
        let mut stream_error: Option<String> = None;
        let mut saw_message_stop = false;
        let mut saw_final_usage = false;

        while let Some(raw_line) = lines.next_line().await? {
            let line = raw_line.trim();

            if line.is_empty() {
                continue;
            }

            // Track event type from `event:` lines
            if let Some(event_type) = sse_field(line, "event") {
                current_event_type = event_type.trim().to_string();
                continue;
            }

            // Parse `data:` lines
            let Some(data) = sse_field(line, "data") else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<Value>(data) else {
                continue;
            };

            match current_event_type.as_str() {
                "message_start" => {
                    accounting.observe(parsed["message"].get("usage"), false);
                    if let Some(usage) = parsed.get("message").and_then(|m| m.get("usage")) {
                        input_tokens = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
                        cached_tokens =
                            usage["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32;
                    }
                }
                "content_block_start" => {
                    let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                    let block = &parsed["content_block"];
                    let block_type = block["type"].as_str().unwrap_or("text").to_string();

                    if block_type == "tool_use" {
                        let id = block["id"].as_str().unwrap_or("").to_string();
                        let name = block["name"].as_str().unwrap_or("").to_string();
                        let _ = tx
                            .send(StreamChunk::ToolCallStart {
                                id: id.clone(),
                                name: name.clone(),
                            })
                            .await;
                        block_states.insert(
                            index,
                            BlockState {
                                block_type,
                                id,
                                name,
                                text: String::new(),
                            },
                        );
                    } else {
                        block_states.insert(
                            index,
                            BlockState {
                                block_type,
                                id: String::new(),
                                name: String::new(),
                                text: String::new(),
                            },
                        );
                    }
                }
                "content_block_delta" => {
                    let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &parsed["delta"];
                    let delta_type = delta["type"].as_str().unwrap_or("");

                    match delta_type {
                        "text_delta" => {
                            if let Some(text) = delta["text"].as_str() {
                                crate::utils::http::bounded_push_str(
                                    &mut full_content,
                                    text,
                                    &mut stored_output_bytes,
                                    output_limit,
                                    "Anthropic accumulated output",
                                )
                                .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                                let _ = tx.send(StreamChunk::Text(text.to_string())).await;
                                if let Some(state) = block_states.get_mut(&index) {
                                    crate::utils::http::bounded_push_str(
                                        &mut state.text,
                                        text,
                                        &mut stored_output_bytes,
                                        output_limit,
                                        "Anthropic accumulated output",
                                    )
                                    .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(partial) = delta["partial_json"].as_str()
                                && let Some(state) = block_states.get_mut(&index)
                            {
                                crate::utils::http::bounded_push_str(
                                    &mut state.text,
                                    partial,
                                    &mut stored_output_bytes,
                                    output_limit,
                                    "Anthropic accumulated output",
                                )
                                .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                                let _ = tx
                                    .send(StreamChunk::ToolCallDelta {
                                        id: state.id.clone(),
                                        arguments_delta: partial.to_string(),
                                    })
                                    .await;
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta["thinking"].as_str() {
                                crate::utils::http::bounded_push_str(
                                    &mut full_reasoning,
                                    text,
                                    &mut stored_output_bytes,
                                    output_limit,
                                    "Anthropic accumulated output",
                                )
                                .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                                let _ = tx.send(StreamChunk::Thinking(text.to_string())).await;
                            }
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    // Block finalized — state already tracked
                }
                "message_delta" => {
                    saw_final_usage |= parsed["usage"].get("output_tokens").is_some();
                    accounting.observe(parsed.get("usage"), false);
                    if let Some(usage) = parsed.get("usage") {
                        output_tokens = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
                    }
                }
                "message_stop" => {
                    saw_message_stop = true;
                    accounting.observe(None, saw_final_usage);
                    let _ = tx.send(StreamChunk::Done).await;
                }
                "error" => {
                    // e.g. {"type":"error","error":{"type":"overloaded_error",
                    //       "message":"Overloaded"}}
                    let err = &parsed["error"];
                    let kind = err["type"].as_str().unwrap_or("error");
                    let msg = err["message"].as_str().unwrap_or("stream error");
                    stream_error = Some(format!("{}: {}", kind, msg));
                    break;
                }
                _ => {}
            }
        }

        // A mid-stream error event, or a stream that ended before `message_stop`,
        // means the response is incomplete — fail rather than return partial text.
        if let Some(err) = stream_error {
            return Err(IronCrewError::Provider(format!(
                "Anthropic stream error — {err}"
            )));
        }
        if !saw_message_stop {
            return Err(IronCrewError::Provider(
                "Anthropic stream ended before message_stop (truncated response)".into(),
            ));
        }

        let (structured_output, tool_calls) =
            output::assemble(block_states, structured_output_tool)?;
        let content = match structured_output {
            Some(json) => Some(json),
            None if full_content.is_empty() => None,
            None => Some(full_content),
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
            usage: Some(TokenUsage {
                prompt_tokens: input_tokens,
                completion_tokens: output_tokens,
                total_tokens: input_tokens + output_tokens,
                cached_tokens,
            }),
            // The streaming path does not reconstruct replayable thinking blocks
            // (that needs the per-block signature reassembled from signature
            // deltas). It isn't required for the tool-use round-trip: the
            // executor forces non-streaming whenever tools are present, and the
            // non-streaming parser above captures the full blocks. If streaming
            // is ever combined with a tool loop, add signature reconstruction
            // here mirroring `parse_anthropic_response`.
            raw_blocks: None,
        })
    }
}

/// State tracked per content block during streaming.
struct BlockState {
    block_type: String,
    id: String,
    name: String,
    text: String, // accumulated text or JSON arguments
}

use super::*;

impl OpenAiResponsesProvider {
    pub(super) async fn send_request_stream(
        &self,
        mut body: Value,
        usage_tracker: Option<&UsageTracker>,
        tx: tokio::sync::mpsc::Sender<StreamChunk>,
    ) -> Result<ChatResponse> {
        body["stream"] = json!(true);
        let request_body = self.prepare_request(&body)?;

        if let Some(ref limiter) = self.rate_limit {
            limiter.wait().await;
        }

        let url = format!("{}/v1/responses", self.base_url);
        crate::utils::network::validate_url_not_private(&url)
            .map_err(|error| IronCrewError::Provider(format!("Unsafe provider URL: {error}")))?;

        let mut accounting = ProviderAttempt::start(usage_tracker, ProviderUsage::OpenAiResponses)?;
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
            return Err(read_error_response(
                resp,
                self.execution_policy,
                "OpenAI Responses error response",
            )
            .await?
            .into_accounted_error(&mut accounting));
        }

        let mut full_content = String::new();
        let mut full_reasoning = String::new();
        let output_limit = self.execution_policy.output_bytes();
        let mut stored_output_bytes = 0_usize;
        let mut item_states: BTreeMap<usize, ItemState> = BTreeMap::new();
        let mut item_indexes: HashMap<String, usize> = HashMap::new();
        let mut usage_data: Option<Value> = None;

        let mut lines =
            ProviderSseLines::new(resp, self.execution_policy, "OpenAI Responses stream");
        let mut current_event_type = String::new();
        // Track terminal delivery: `response.failed`/`error`, or a stream that
        // ends before `response.completed`, must fail rather than return
        // partial content as a successful response.
        let mut stream_error: Option<String> = None;
        let mut saw_completed = false;

        while let Some(raw_line) = lines.next_line().await? {
            let line = raw_line.trim();

            if line.is_empty() {
                continue;
            }

            if let Some(event_type) = sse_field(line, "event") {
                current_event_type = event_type.trim().to_string();
                continue;
            }

            let Some(data) = sse_field(line, "data") else {
                continue;
            };

            if data == "[DONE]" {
                saw_completed = true;
                let _ = tx.send(StreamChunk::Done).await;
                continue;
            }

            let Ok(parsed) = serde_json::from_str::<Value>(data) else {
                continue;
            };

            // Terminal errors/incomplete responses may still have final usage.
            let terminal_usage = matches!(
                current_event_type.as_str(),
                "response.completed" | "response.failed" | "response.incomplete"
            );
            let receipt = parsed["response"]
                .get("usage")
                .or_else(|| parsed.get("usage"));
            accounting.observe(
                receipt,
                terminal_usage && receipt.is_some_and(Value::is_object),
            );
            match current_event_type.as_str() {
                "response.output_item.added" => {
                    let item = &parsed["item"];
                    let item_id = item["id"].as_str().unwrap_or("").to_string();
                    let item_type = item["type"].as_str().unwrap_or("").to_string();
                    let output_index = parsed["output_index"]
                        .as_u64()
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or(item_states.len());
                    item_indexes.insert(item_id, output_index);

                    if item_type == "function_call" {
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                        let _ = tx
                            .send(StreamChunk::ToolCallStart {
                                id: call_id.clone(),
                                name: name.clone(),
                            })
                            .await;
                        item_states.insert(
                            output_index,
                            ItemState {
                                item_type,
                                call_id,
                                name,
                                text: String::new(),
                            },
                        );
                    } else {
                        item_states.insert(
                            output_index,
                            ItemState {
                                item_type,
                                call_id: String::new(),
                                name: String::new(),
                                text: String::new(),
                            },
                        );
                    }
                }
                "response.output_text.delta" => {
                    if let Some(delta) = parsed["delta"].as_str() {
                        crate::utils::http::bounded_push_str(
                            &mut full_content,
                            delta,
                            &mut stored_output_bytes,
                            output_limit,
                            "OpenAI Responses accumulated output",
                        )
                        .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                        let _ = tx.send(StreamChunk::Text(delta.to_string())).await;
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    if let Some(delta) = parsed["delta"].as_str() {
                        crate::utils::http::bounded_push_str(
                            &mut full_reasoning,
                            delta,
                            &mut stored_output_bytes,
                            output_limit,
                            "OpenAI Responses accumulated output",
                        )
                        .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                        let _ = tx.send(StreamChunk::Thinking(delta.to_string())).await;
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(delta) = parsed["delta"].as_str() {
                        let item_id = parsed["item_id"].as_str().unwrap_or("");
                        if let Some(state) = item_indexes
                            .get(item_id)
                            .and_then(|index| item_states.get_mut(index))
                        {
                            crate::utils::http::bounded_push_str(
                                &mut state.text,
                                delta,
                                &mut stored_output_bytes,
                                output_limit,
                                "OpenAI Responses accumulated output",
                            )
                            .map_err(|error| IronCrewError::Provider(error.to_string()))?;
                            let _ = tx
                                .send(StreamChunk::ToolCallDelta {
                                    id: state.call_id.clone(),
                                    arguments_delta: delta.to_string(),
                                })
                                .await;
                        }
                    }
                }
                "response.completed" => {
                    saw_completed = true;
                    if let Some(usage) = parsed["response"].get("usage").cloned() {
                        usage_data = Some(usage);
                    }
                    let _ = tx.send(StreamChunk::Done).await;
                }
                "response.failed" | "response.incomplete" | "error" => {
                    let err_msg = parsed["error"]["message"]
                        .as_str()
                        .or_else(|| parsed["response"]["error"]["message"].as_str())
                        .unwrap_or("Responses API stream error");
                    let _ = tx.send(StreamChunk::Error(err_msg.to_string())).await;
                    stream_error = Some(err_msg.to_string());
                    break;
                }
                _ => {}
            }
        }

        if let Some(err) = stream_error {
            return Err(IronCrewError::Provider(format!(
                "OpenAI Responses stream error — {err}"
            )));
        }
        if !saw_completed {
            return Err(IronCrewError::Provider(
                "OpenAI Responses stream ended before response.completed (truncated response)"
                    .into(),
            ));
        }

        // Assemble tool calls from item states
        let tool_calls: Vec<ToolCallRequest> = item_states
            .into_iter()
            .filter(|(_, state)| state.item_type == "function_call")
            .map(|(index, state)| {
                if state.call_id.is_empty() || state.name.is_empty() {
                    return Err(IronCrewError::Provider(format!(
                        "OpenAI Responses stream ended with incomplete tool call at index {index}"
                    )));
                }
                Ok(ToolCallRequest {
                    id: state.call_id,
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: state.name,
                        arguments: state.text,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;

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

        let usage = usage_data.map(|u| TokenUsage {
            prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
            cached_tokens: u["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0) as u32,
        });

        accounting.finish()?;
        Ok(ChatResponse {
            content,
            reasoning,
            tool_calls,
            usage,
            // Streaming doesn't reassemble full reasoning items (with
            // encrypted_content) for replay. Not needed for the tool-use
            // round-trip: the executor forces non-streaming when tools are
            // present, and `parse_responses_response` captures them there.
            raw_blocks: None,
        })
    }
}

/// State tracked per output item during streaming.
struct ItemState {
    item_type: String,
    call_id: String,
    name: String,
    text: String,
}

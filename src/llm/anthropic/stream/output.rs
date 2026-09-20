use super::*;

pub(super) fn assemble(
    block_states: BTreeMap<usize, BlockState>,
    structured_output_tool: Option<&str>,
) -> Result<(Option<String>, Vec<ToolCallRequest>)> {
    // Assemble tool calls from block states. A forced structured-output
    // tool carries the answer itself, so its accumulated JSON becomes the
    // response content rather than a tool call.
    let mut structured_output: Option<String> = None;
    let mut tool_calls = Vec::new();
    for (index, state) in block_states {
        if state.block_type != "tool_use" {
            continue;
        }
        if state.id.is_empty() || state.name.is_empty() {
            return Err(IronCrewError::Provider(format!(
                "Anthropic stream ended with incomplete tool call at index {index}"
            )));
        }
        if structured_output_tool == Some(state.name.as_str()) {
            structured_output = Some(state.text);
            continue;
        }
        tool_calls.push(ToolCallRequest {
            id: state.id,
            call_type: "function".to_string(),
            function: ToolCallFunction {
                name: state.name,
                arguments: state.text,
            },
        });
    }

    if structured_output_tool.is_some() && structured_output.is_none() && tool_calls.is_empty() {
        return Err(IronCrewError::Provider(
            "Anthropic response omitted the required structured-output tool".into(),
        ));
    }
    Ok((structured_output, tool_calls))
}

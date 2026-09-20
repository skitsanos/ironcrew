use super::*;

impl AgentDialog {
    pub(super) async fn execute_tool_call(
        &self,
        tool_call: &ToolCallRequest,
        caller_agent: &str,
        turn_idx: usize,
    ) -> String {
        let args = match crate::llm::tool_arguments::parse(&tool_call.function.arguments) {
            Ok(args) => args,
            Err(result) => return result.to_string(),
        };

        let tool_timeout = self
            .tool_registry
            .dispatch_timeout(&tool_call.function.name, &args)
            .unwrap_or_else(|| Duration::from_secs(crate::lua::agent_turn::tool_timeout_secs()));

        // Pass the dialog store and event bus to custom tools. The scoped turn
        // identifier attributes nested agent and sub-flow events precisely.
        let tool_ctx = ToolCallContext {
            usage_tracker: self.provider.usage_tracker(),
            store: self.store.clone(),
            eventbus: Some(self.eventbus.clone()),
            depth: 0,
            tool_registry: Some(self.tool_registry.clone()),
            caller_agent: Some(caller_agent.to_string()),
            caller_scope: Some(format!("{}:t{}", self.id, turn_idx)),
            // Dialogs don't carry a human-input transport yet — human
            // steering happens via should_stop/turn_selector callbacks
            // (which run in flow scope and can call crew:ask_human).
            ask_human: None,
        };

        let tool_result = match tokio::time::timeout(
            tool_timeout,
            self.tool_registry
                .execute(&tool_call.function.name, args, &tool_ctx),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(IronCrewError::ToolExecution {
                tool: tool_call.function.name.clone(),
                message: format!("Tool timed out after {}s", tool_timeout.as_secs()),
            }),
        };

        match tool_result {
            Ok(output) => output,
            Err(e) => format!("Tool error: {}", e),
        }
    }
}

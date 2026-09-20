/// Anthropic-specific configuration (server-side tools, extended thinking).
#[derive(Debug, Clone, Default)]
pub struct AnthropicConfig {
    /// Extended thinking budget in tokens; None = disabled.
    pub thinking_budget: Option<u32>,
    /// Server-side tools to include in every request.
    pub server_tools: Vec<ServerTool>,
}

/// Anthropic server-side tools (executed by Anthropic, not locally).
#[derive(Debug, Clone)]
pub enum ServerTool {
    WebSearch { max_uses: Option<u32> },
    CodeExecution,
}

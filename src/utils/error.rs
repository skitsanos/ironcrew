use thiserror::Error;

#[derive(Error, Debug)]
pub enum IronCrewError {
    #[error("LLM provider error: {0}")]
    Provider(String),

    #[error("Tool execution error: {tool}: {message}")]
    ToolExecution { tool: String, message: String },

    #[error("Lua error: {0}")]
    Lua(#[from] mlua::Error),

    #[error("Task error: {task}: {message}")]
    Task { task: String, message: String },

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    /// A keyed run claim was durably fenced while its owner entered drain.
    /// Callers must not start new physical execution for this claim.
    #[error("Run owner instance '{owner_instance_id}' is draining")]
    OwnerDraining { owner_instance_id: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[cfg(feature = "mcp")]
    #[error("MCP error [{server}]: {message}")]
    Mcp { server: String, message: String },
}

pub type Result<T> = std::result::Result<T, IronCrewError>;

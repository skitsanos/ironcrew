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

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, IronCrewError>;

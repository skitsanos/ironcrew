use async_trait::async_trait;
use serde_json::json;
use tokio::process::Command;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

pub struct ShellTool;

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its output"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "shell".into(),
            description: "Execute a shell command and return its output".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "shell".into(),
                message: "Missing 'command' argument".into(),
            })?;

        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await
            .map_err(|e| IronCrewError::ToolExecution {
                tool: "shell".into(),
                message: format!("Failed to execute: {}", e),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            Ok(format!("Exit code: {}\nStdout: {}\nStderr: {}", output.status, stdout, stderr))
        }
    }
}

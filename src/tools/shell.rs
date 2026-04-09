use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

/// Read from an async reader into a byte buffer until `max` bytes are
/// collected. If `max` is reached, discard the rest of the stream and set
/// `truncated` to true. Returns (bytes, truncated).
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    max: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::with_capacity(1024.min(max));
    let mut tmp = [0u8; 4096];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        if buf.len() >= max {
            truncated = true;
            // Keep reading and discard so the child process can exit cleanly.
            continue;
        }
        let take = (max - buf.len()).min(n);
        buf.extend_from_slice(&tmp[..take]);
        if take < n {
            truncated = true;
        }
    }
    Ok((buf, truncated))
}

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

        // Cap each output stream independently (default 1 MB per stream).
        let max_output: usize = std::env::var("IRONCREW_SHELL_MAX_OUTPUT_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1024 * 1024);

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| IronCrewError::ToolExecution {
                tool: "shell".into(),
                message: format!("Failed to spawn: {}", e),
            })?;

        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "shell".into(),
                message: "Failed to capture stdout".into(),
            })?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| IronCrewError::ToolExecution {
                tool: "shell".into(),
                message: "Failed to capture stderr".into(),
            })?;

        // Read both streams concurrently with independent byte caps.
        let (stdout_result, stderr_result, status) = tokio::join!(
            read_bounded(BufReader::new(stdout_pipe), max_output),
            read_bounded(BufReader::new(stderr_pipe), max_output),
            child.wait()
        );

        let (stdout_bytes, stdout_truncated) =
            stdout_result.map_err(|e| IronCrewError::ToolExecution {
                tool: "shell".into(),
                message: format!("Failed to read stdout: {}", e),
            })?;
        let (stderr_bytes, stderr_truncated) =
            stderr_result.map_err(|e| IronCrewError::ToolExecution {
                tool: "shell".into(),
                message: format!("Failed to read stderr: {}", e),
            })?;
        let status = status.map_err(|e| IronCrewError::ToolExecution {
            tool: "shell".into(),
            message: format!("Failed to wait for process: {}", e),
        })?;

        let mut stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        if stdout_truncated {
            stdout.push_str(&format!(
                "\n[stdout truncated at {} bytes — set IRONCREW_SHELL_MAX_OUTPUT_BYTES to override]",
                max_output
            ));
        }
        let mut stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
        if stderr_truncated {
            stderr.push_str(&format!(
                "\n[stderr truncated at {} bytes — set IRONCREW_SHELL_MAX_OUTPUT_BYTES to override]",
                max_output
            ));
        }

        if status.success() {
            Ok(stdout)
        } else {
            Ok(format!(
                "Exit code: {}\nStdout: {}\nStderr: {}",
                status, stdout, stderr
            ))
        }
    }
}

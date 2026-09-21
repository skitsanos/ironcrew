use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

use super::{Tool, ToolCallContext};
use crate::llm::provider::ToolSchema;
use crate::utils::error::{IronCrewError, Result};

mod environment;
mod policy;
use policy::{MAX_TIMEOUT_SECS, ShellPolicy};

const HARD_MAX_COMMAND_BYTES: usize = 64 * 1024;
const TERMINATION_GRACE: Duration = Duration::from_millis(500);
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

fn shell_error(message: impl Into<String>) -> IronCrewError {
    IronCrewError::ToolExecution {
        tool: "shell".into(),
        message: message.into(),
    }
}

/// Process-group cleanup for cancellation and drop paths.
struct ProcessGroupGuard {
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl ProcessGroupGuard {
    fn new(process_id: Option<u32>) -> Self {
        #[cfg(not(unix))]
        let _ = process_id;
        Self {
            #[cfg(unix)]
            pgid: process_id.and_then(|id| i32::try_from(id).ok()),
        }
    }

    fn terminate(&self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }

    fn kill_and_disarm(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.kill_and_disarm();
    }
}

/// Read at most `max` bytes while draining the stream to let the child exit.
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

pub struct ShellTool {
    policy: ShellPolicy,
}

impl Default for ShellTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellTool {
    pub fn new() -> Self {
        Self {
            policy: ShellPolicy::capture(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_policy_for_test(timeout_secs: u64, max_output_bytes: usize) -> Self {
        Self {
            policy: ShellPolicy::from_values(timeout_secs, max_output_bytes),
        }
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
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT_SECS,
                        "description": "Command deadline in seconds (default 60, maximum 3600)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn conversation_definition(&self) -> Result<serde_json::Value> {
        Ok(json!({
            "schema": self.schema(),
            "policy": self.policy.definition()?,
        }))
    }

    fn dispatch_timeout(&self, args: &serde_json::Value) -> Option<Duration> {
        // Let the shell's own timeout path terminate and reap the process group
        // before the generic dispatcher cancels this future.
        self.policy
            .requested_timeout(args)
            .ok()
            .and_then(|timeout| timeout.checked_add(READER_DRAIN_GRACE + TERMINATION_GRACE))
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| shell_error("Missing 'command' argument"))?;
        if command.is_empty() || command.len() > HARD_MAX_COMMAND_BYTES {
            return Err(shell_error(format!(
                "'command' must contain 1..={HARD_MAX_COMMAND_BYTES} bytes"
            )));
        }

        let timeout = self.policy.requested_timeout(&args)?;

        // Cap each output stream independently (default 1 MB per stream).
        let max_output = self.policy.max_output_bytes()?;

        let mut command_builder = Command::new("sh");
        command_builder
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Model-controlled commands must not inherit provider keys or other
        // process secrets; start from a minimal allowlist instead.
        command_builder
            .env_clear()
            .envs(environment::child_environment());
        #[cfg(unix)]
        command_builder.process_group(0);

        let mut child = command_builder
            .spawn()
            .map_err(|e| shell_error(format!("Failed to spawn: {e}")))?;
        let mut process_group = ProcessGroupGuard::new(child.id());

        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| shell_error("Failed to capture stdout"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| shell_error("Failed to capture stderr"))?;

        let mut stdout_task = tokio::spawn(read_bounded(BufReader::new(stdout_pipe), max_output));
        let mut stderr_task = tokio::spawn(read_bounded(BufReader::new(stderr_pipe), max_output));

        let wait = tokio::time::timeout(timeout, child.wait()).await;
        let (status, timed_out) = match wait {
            Ok(Ok(status)) => (Some(status), false),
            Ok(Err(error)) => {
                process_group.kill_and_disarm();
                return Err(shell_error(format!("Failed to wait for process: {error}")));
            }
            Err(_) => {
                process_group.terminate();
                if tokio::time::timeout(TERMINATION_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    process_group.kill_and_disarm();
                    let _ = child.kill().await;
                }
                (None, true)
            }
        };

        // Close descendant-held output descriptors before draining.
        process_group.kill_and_disarm();

        let reader_results = tokio::time::timeout(READER_DRAIN_GRACE, async {
            let stdout = (&mut stdout_task).await;
            let stderr = (&mut stderr_task).await;
            (stdout, stderr)
        })
        .await;
        let (stdout_result, stderr_result) = match reader_results {
            Ok((stdout, stderr)) => (
                stdout.map_err(|error| shell_error(format!("stdout reader failed: {error}")))?,
                stderr.map_err(|error| shell_error(format!("stderr reader failed: {error}")))?,
            ),
            Err(_) => {
                stdout_task.abort();
                stderr_task.abort();
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(shell_error(
                    "Timed out while closing command output streams",
                ));
            }
        };

        let (stdout_bytes, stdout_truncated) =
            stdout_result.map_err(|e| shell_error(format!("Failed to read stdout: {e}")))?;
        let (stderr_bytes, stderr_truncated) =
            stderr_result.map_err(|e| shell_error(format!("Failed to read stderr: {e}")))?;

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

        if timed_out {
            return Err(shell_error(format!(
                "Command timed out after {} seconds; stdout: {}; stderr: {}",
                timeout.as_secs(),
                stdout,
                stderr
            )));
        }

        let status = status.expect("non-timeout wait must produce a status");

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

#[cfg(test)]
mod tests;

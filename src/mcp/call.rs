//! Bounded MCP 2026-07-28 tool-call lifecycle.

use std::time::Duration;

use rmcp::{
    Peer, RoleClient,
    model::{CallToolRequestParams, CallToolResponse, CallToolResult},
};

use crate::mcp::execution_policy::McpCallPolicy;
use crate::utils::error::{IronCrewError, Result};

pub(super) async fn call_tool(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    name: &str,
    policy: McpCallPolicy,
) -> Result<CallToolResult> {
    let timeout = policy.timeout();
    tokio::time::timeout(timeout, drive_call(peer, params, name, policy))
        .await
        .map_err(|_| {
            mcp_error(format!(
                "call_tool '{name}' timed out after {} seconds",
                timeout.as_secs()
            ))
        })?
}

async fn drive_call(
    peer: &Peer<RoleClient>,
    mut params: CallToolRequestParams,
    name: &str,
    policy: McpCallPolicy,
) -> Result<CallToolResult> {
    let max_rounds = policy.max_mrtr_rounds();

    for round in 0..max_rounds {
        match peer
            .call_tool_once(params.clone())
            .await
            .map_err(|error| mcp_error(format!("call_tool '{name}' failed: {error}")))?
        {
            CallToolResponse::Complete(result) => return Ok(result),
            CallToolResponse::InputRequired(result) => {
                if result
                    .input_requests
                    .as_ref()
                    .is_some_and(|requests| !requests.is_empty())
                {
                    return Err(mcp_error(format!(
                        "call_tool '{name}' returned inputRequests for capabilities IronCrew did not advertise"
                    )));
                }

                let Some(request_state) = result.request_state else {
                    return Err(mcp_error(format!(
                        "call_tool '{name}' returned input_required without usable inputRequests or requestState"
                    )));
                };
                policy.validate_request_state(&request_state)?;

                if round + 1 == max_rounds {
                    return Err(mcp_error(format!(
                        "call_tool '{name}' exceeded the {max_rounds}-round MRTR limit"
                    )));
                }

                tokio::time::sleep(state_only_backoff(round)).await;
                params.input_responses = None;
                params.request_state = Some(request_state);
            }
            CallToolResponse::Task(_) => {
                return Err(mcp_error(format!(
                    "call_tool '{name}' returned a task without the io.modelcontextprotocol/tasks capability"
                )));
            }
            _ => {
                return Err(mcp_error(format!(
                    "call_tool '{name}' returned an unsupported response variant"
                )));
            }
        }
    }

    Err(mcp_error(format!(
        "call_tool '{name}' exceeded the {max_rounds}-round MRTR limit"
    )))
}

fn state_only_backoff(round: usize) -> Duration {
    let multiplier = 1_u64 << round.min(3);
    Duration::from_millis((50_u64.saturating_mul(multiplier)).min(250))
}

fn mcp_error(message: impl Into<String>) -> IronCrewError {
    IronCrewError::Mcp {
        server: String::new(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_only_backoff_is_bounded() {
        assert_eq!(state_only_backoff(0), Duration::from_millis(50));
        assert_eq!(state_only_backoff(1), Duration::from_millis(100));
        assert_eq!(state_only_backoff(2), Duration::from_millis(200));
        assert_eq!(state_only_backoff(3), Duration::from_millis(250));
        assert_eq!(state_only_backoff(usize::MAX), Duration::from_millis(250));
    }
}

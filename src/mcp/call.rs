//! Bounded, cancellable MCP 2026 request lifecycle.

use std::time::Duration;

use rmcp::{
    Peer, RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, ClientRequest,
        ListToolsRequest, ListToolsResult, PaginatedRequestParams, ServerResult,
    },
    service::{PeerRequestOptions, ServiceError},
};
use tokio::time::{Instant, sleep};

use crate::mcp::connection::InFlightGuard;
use crate::mcp::execution_policy::McpCallPolicy;
use crate::utils::error::{IronCrewError, Result};

pub(super) async fn list_tools(
    peer: &Peer<RoleClient>,
    params: Option<PaginatedRequestParams>,
    name: &str,
    deadline: Instant,
    guard: &InFlightGuard,
) -> Result<ListToolsResult> {
    let request = ClientRequest::ListToolsRequest(ListToolsRequest {
        method: Default::default(),
        params,
        extensions: Default::default(),
    });
    match send_request(peer, request, name, deadline, guard).await? {
        ServerResult::ListToolsResult(result) => Ok(result),
        _ => Err(mcp_error(format!(
            "{name} returned an unexpected response type"
        ))),
    }
}

pub(super) async fn call_tool(
    peer: &Peer<RoleClient>,
    mut params: CallToolRequestParams,
    name: &str,
    policy: McpCallPolicy,
    guard: &InFlightGuard,
) -> Result<CallToolResult> {
    let deadline = Instant::now() + policy.timeout();
    let max_rounds = policy.max_mrtr_rounds();

    for round in 0..max_rounds {
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params.clone()));
        let response = match send_request(
            peer,
            request,
            &format!("call_tool '{name}'"),
            deadline,
            guard,
        )
        .await?
        {
            ServerResult::CallToolResult(result) => CallToolResponse::Complete(result),
            ServerResult::InputRequiredResult(result) => CallToolResponse::InputRequired(result),
            ServerResult::CreateTaskResult(result) => CallToolResponse::Task(result),
            _ => {
                guard.poison();
                return Err(mcp_error(format!(
                    "call_tool '{name}' returned an unsupported response variant"
                )));
            }
        };

        match response {
            CallToolResponse::Complete(result) => return Ok(result),
            CallToolResponse::InputRequired(result) => {
                if result
                    .input_requests
                    .as_ref()
                    .is_some_and(|requests| !requests.is_empty())
                {
                    guard.poison();
                    return Err(mcp_error(format!(
                        "call_tool '{name}' returned inputRequests for capabilities IronCrew did not advertise"
                    )));
                }
                let Some(request_state) = result.request_state else {
                    guard.poison();
                    return Err(mcp_error(format!(
                        "call_tool '{name}' returned input_required without usable inputRequests or requestState"
                    )));
                };
                if let Err(error) = policy.validate_request_state(&request_state) {
                    guard.poison();
                    return Err(error);
                }
                if round + 1 == max_rounds {
                    guard.poison();
                    return Err(mcp_error(format!(
                        "call_tool '{name}' exceeded the {max_rounds}-round MRTR limit"
                    )));
                }
                sleep_with_deadline(state_only_backoff(round), deadline, name).await?;
                params.input_responses = None;
                params.request_state = Some(request_state);
            }
            CallToolResponse::Task(_) => {
                guard.poison();
                return Err(mcp_error(format!(
                    "call_tool '{name}' returned a task without the io.modelcontextprotocol/tasks capability"
                )));
            }
            _ => {
                guard.poison();
                return Err(mcp_error(format!(
                    "call_tool '{name}' returned an unsupported response variant"
                )));
            }
        }
    }
    guard.poison();
    Err(mcp_error(format!(
        "call_tool '{name}' exceeded the {max_rounds}-round MRTR limit"
    )))
}

async fn send_request(
    peer: &Peer<RoleClient>,
    request: ClientRequest,
    name: &str,
    deadline: Instant,
    guard: &InFlightGuard,
) -> Result<ServerResult> {
    let remaining = remaining(deadline, name)?;
    let cancellation_budget = (remaining / 10).min(Duration::from_millis(100));
    let response_budget = remaining.saturating_sub(cancellation_budget);
    let options =
        PeerRequestOptions::with_timeout(response_budget).with_max_total_timeout(response_budget);
    let handle = tokio::time::timeout(remaining, peer.send_cancellable_request(request, options))
        .await
        .map_err(|_| timeout_error(name))?
        .map_err(|error| mcp_error(format!("{name} failed: {error}")))?;
    let response = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => {
            guard.poison();
            return Err(timeout_error(name));
        }
        response = handle.await_response() => response,
    };
    match response {
        Ok(result) => Ok(result),
        Err(ServiceError::Timeout { .. }) => {
            guard.poison_and_wait(deadline).await;
            Err(timeout_error(name))
        }
        Err(error) => Err(mcp_error(format!("{name} failed: {error}"))),
    }
}

async fn sleep_with_deadline(delay: Duration, deadline: Instant, name: &str) -> Result<()> {
    let remaining = remaining(deadline, &format!("call_tool '{name}'"))?;
    if delay >= remaining {
        return Err(timeout_error(&format!("call_tool '{name}'")));
    }
    sleep(delay).await;
    Ok(())
}

fn remaining(deadline: Instant, name: &str) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| timeout_error(name))
}

fn timeout_error(name: &str) -> IronCrewError {
    mcp_error(format!("{name} reached its configured deadline"))
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

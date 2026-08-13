//! Bounded, cancellable MCP 2026 request lifecycle.

use std::time::Duration;

use rmcp::{
    Peer, RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, ClientRequest,
        ErrorCode, ListToolsRequest, ListToolsResult, PaginatedRequestParams, ServerResult,
    },
    service::{PeerRequestOptions, ServiceError},
};
use tokio::time::{Instant, sleep};

use crate::mcp::connection::InFlightGuard;
use crate::mcp::execution_policy::McpCallPolicy;
use crate::utils::error::{IronCrewError, Result};

pub(super) enum CallToolFailure {
    HeaderMismatch(IronCrewError),
    Other(IronCrewError),
}

impl CallToolFailure {
    pub(super) fn into_error(self) -> IronCrewError {
        match self {
            Self::HeaderMismatch(error) | Self::Other(error) => error,
        }
    }
}

enum RequestFailure {
    HeaderMismatch(IronCrewError),
    Other(IronCrewError),
}

impl RequestFailure {
    fn into_error(self) -> IronCrewError {
        match self {
            Self::HeaderMismatch(error) | Self::Other(error) => error,
        }
    }
}

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
    match send_request(peer, request, name, deadline, guard)
        .await
        .map_err(RequestFailure::into_error)?
    {
        ServerResult::ListToolsResult(result) => Ok(result),
        _ => {
            guard.poison();
            Err(mcp_error(format!(
                "{name} returned an unexpected response type"
            )))
        }
    }
}

pub(super) async fn call_tool(
    peer: &Peer<RoleClient>,
    params: &mut CallToolRequestParams,
    name: &str,
    policy: McpCallPolicy,
    deadline: Instant,
    attempts: &mut usize,
    guard: &InFlightGuard,
) -> std::result::Result<CallToolResult, CallToolFailure> {
    let max_rounds = policy.max_mrtr_rounds();

    loop {
        if *attempts >= max_rounds {
            guard.poison();
            return Err(CallToolFailure::Other(mcp_error(format!(
                "call_tool '{name}' exceeded the {max_rounds}-round MRTR limit"
            ))));
        }
        let round = *attempts;
        *attempts += 1;
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params.clone()));
        let response = match send_request(
            peer,
            request,
            &format!("call_tool '{name}'"),
            deadline,
            guard,
        )
        .await
        {
            Err(RequestFailure::HeaderMismatch(error)) => {
                return Err(CallToolFailure::HeaderMismatch(error));
            }
            Err(RequestFailure::Other(error)) => return Err(CallToolFailure::Other(error)),
            Ok(result) => match result {
                ServerResult::CallToolResult(result) => CallToolResponse::Complete(result),
                ServerResult::InputRequiredResult(result) => {
                    CallToolResponse::InputRequired(result)
                }
                ServerResult::CreateTaskResult(result) => CallToolResponse::Task(result),
                _ => {
                    guard.poison();
                    return Err(CallToolFailure::Other(mcp_error(format!(
                        "call_tool '{name}' returned an unsupported response variant"
                    ))));
                }
            },
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
                    return Err(CallToolFailure::Other(mcp_error(format!(
                        "call_tool '{name}' returned inputRequests for capabilities IronCrew did not advertise"
                    ))));
                }
                let Some(request_state) = result.request_state else {
                    guard.poison();
                    return Err(CallToolFailure::Other(mcp_error(format!(
                        "call_tool '{name}' returned input_required without usable inputRequests or requestState"
                    ))));
                };
                if let Err(error) = policy.validate_request_state(&request_state) {
                    guard.poison();
                    return Err(CallToolFailure::Other(error));
                }
                if *attempts == max_rounds {
                    guard.poison();
                    return Err(CallToolFailure::Other(mcp_error(format!(
                        "call_tool '{name}' exceeded the {max_rounds}-round MRTR limit"
                    ))));
                }
                sleep_with_deadline(state_only_backoff(round), deadline, name, guard)
                    .await
                    .map_err(CallToolFailure::Other)?;
                params.input_responses = None;
                params.request_state = Some(request_state);
            }
            CallToolResponse::Task(_) => {
                guard.poison();
                return Err(CallToolFailure::Other(mcp_error(format!(
                    "call_tool '{name}' returned a task without the io.modelcontextprotocol/tasks capability"
                ))));
            }
            _ => {
                guard.poison();
                return Err(CallToolFailure::Other(mcp_error(format!(
                    "call_tool '{name}' returned an unsupported response variant"
                ))));
            }
        }
    }
}

async fn send_request(
    peer: &Peer<RoleClient>,
    request: ClientRequest,
    name: &str,
    deadline: Instant,
    guard: &InFlightGuard,
) -> std::result::Result<ServerResult, RequestFailure> {
    let Some(remaining) = remaining(deadline) else {
        guard.poison_and_wait(deadline).await;
        return Err(RequestFailure::Other(timeout_error(name)));
    };
    let cancellation_budget = (remaining / 10).min(Duration::from_millis(100));
    let response_budget = remaining.saturating_sub(cancellation_budget);
    let options =
        PeerRequestOptions::with_timeout(response_budget).with_max_total_timeout(response_budget);
    let handle = match tokio::time::timeout(
        remaining,
        peer.send_cancellable_request(request, options),
    )
    .await
    {
        Ok(result) => result
            .map_err(|error| RequestFailure::Other(mcp_error(format!("{name} failed: {error}"))))?,
        Err(_) => {
            guard.poison_and_wait(deadline).await;
            return Err(RequestFailure::Other(timeout_error(name)));
        }
    };
    let response = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => {
            guard.poison();
            return Err(RequestFailure::Other(timeout_error(name)));
        }
        response = handle.await_response() => response,
    };
    match response {
        Ok(result) => Ok(result),
        Err(ServiceError::McpError(error)) if error.code == ErrorCode::HEADER_MISMATCH => Err(
            RequestFailure::HeaderMismatch(mcp_error(format!("{name} failed: {error}"))),
        ),
        Err(ServiceError::Timeout { .. }) => {
            guard.poison_and_wait(deadline).await;
            Err(RequestFailure::Other(timeout_error(name)))
        }
        Err(error) => Err(RequestFailure::Other(mcp_error(format!(
            "{name} failed: {error}"
        )))),
    }
}

async fn sleep_with_deadline(
    delay: Duration,
    deadline: Instant,
    name: &str,
    guard: &InFlightGuard,
) -> Result<()> {
    let Some(remaining) = remaining(deadline) else {
        guard.poison_and_wait(deadline).await;
        return Err(timeout_error(&format!("call_tool '{name}'")));
    };
    if delay >= remaining {
        guard.poison_and_wait(deadline).await;
        return Err(timeout_error(&format!("call_tool '{name}'")));
    }
    sleep(delay).await;
    Ok(())
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
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

//! Bounded pre-deserialization MCP HTTP JSON response handling.

use futures::StreamExt;
use rmcp::{
    model::ServerJsonRpcMessage,
    transport::streamable_http_client::{StreamableHttpError, StreamableHttpError::Client},
};

use super::{
    connection::PoisonWatch, http_transport::StrictHttpError, protocol::inbound_is_allowed,
};

pub(super) async fn bounded_json(
    response: reqwest::Response,
    limit: usize,
    poison: PoisonWatch,
) -> Result<ServerJsonRpcMessage, StreamableHttpError<StrictHttpError>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        poison.poison();
        return Err(Client(StrictHttpError::MessageTooLarge(limit)));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    let mut watch = poison.clone();
    loop {
        let next = tokio::select! {
            biased;
            _ = watch.poisoned() => return Err(Client(StrictHttpError::Poisoned)),
            next = stream.next() => next,
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                poison.poison();
                return Err(Client(StrictHttpError::Request(error)));
            }
        };
        if body.len().saturating_add(chunk.len()) > limit {
            poison.poison();
            return Err(Client(StrictHttpError::MessageTooLarge(limit)));
        }
        body.extend_from_slice(&chunk);
    }
    let message = serde_json::from_slice::<ServerJsonRpcMessage>(&body).map_err(|error| {
        poison.poison();
        StreamableHttpError::Deserialize(error)
    })?;
    if !inbound_is_allowed(&message) {
        poison.poison();
        return Err(Client(StrictHttpError::ProtocolDirection));
    }
    Ok(message)
}

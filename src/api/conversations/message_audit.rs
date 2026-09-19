use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::{
    Json,
    extract::{ConnectInfo, Extension, Path, State},
    http::{HeaderMap, StatusCode},
};

use super::{MessageReq, MessageResult, post_message_inner};
use crate::api::AppState;
use crate::api::auth::Principal;

/// Audit the complete message-handler boundary. Idempotent execution keeps its
/// audit inside the detached task so a disconnected client cannot cancel the
/// record; `audit_recorded` prevents the boundary from writing it twice.
pub async fn post_message(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
    Path((flow, id)): Path<(String, String)>,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<MessageReq>,
) -> MessageResult {
    let audit_recorded = Arc::new(AtomicBool::new(false));
    let audit_headers = crate::api::audit::background_headers(&headers);
    let idempotent = headers.contains_key(crate::api::idempotency::IDEMPOTENCY_KEY_HEADER);
    let result = post_message_inner(
        State(state.clone()),
        Extension(principal),
        Path((flow.clone(), id.clone())),
        headers,
        ConnectInfo(addr),
        Json(req),
        audit_recorded.clone(),
    )
    .await;

    if !audit_recorded.load(Ordering::Acquire) {
        let (success, status_code, metadata) = match &result {
            Ok((_, Json(response))) => (
                true,
                StatusCode::OK.as_u16(),
                Some(serde_json::json!({
                    "idempotent": false,
                    "turn_index": response.turn_index,
                    "turn_count": response.turn_count,
                })),
            ),
            Err((status, _)) => (
                false,
                status.as_u16(),
                Some(serde_json::json!({ "idempotent": idempotent })),
            ),
        };
        crate::api::audit::record(
            &state.store,
            "conversation.message",
            Some(&flow),
            Some(&id),
            &audit_headers,
            Some(addr),
            success,
            status_code,
            metadata,
        )
        .await;
    }

    result
}

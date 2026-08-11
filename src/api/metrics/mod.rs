//! Protected Prometheus endpoint and fixed-cardinality exposition.

mod idempotency;
mod process;

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::auth::Principal;

/// Protected Prometheus text exposition. Labels are fixed and deliberately do
/// not include principal ids, audit actors, idempotency keys, flow names, or
/// any other attacker-controlled/high-cardinality value.
pub(super) async fn metrics(
    State(state): State<Arc<AppState>>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let process_snapshot = process::Snapshot::capture(&state).await;
    let durable_usage = match state
        .admission
        .cached_idempotency_usage(
            state.store.as_ref(),
            principal.id(),
            state.idempotency.limits(),
        )
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            crate::metrics::record_store_failure(crate::metrics::StoreOperation::MetricsSnapshot);
            tracing::warn!(%error, "Failed to read idempotency saturation metrics");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                axum::Json(serde_json::json!({
                    "error": "Metrics storage snapshot is temporarily unavailable"
                })),
            )
                .into_response();
        }
    };

    let mut body = String::with_capacity(4 * 1024);
    process::append(&mut body, &state, process_snapshot).await;
    idempotency::append(&mut body, durable_usage, state.idempotency.limits());
    crate::metrics::append_prometheus(&mut body);

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn write_gauge<T: std::fmt::Display>(body: &mut String, name: &str, value: T) {
    writeln!(body, "# TYPE {name} gauge").unwrap();
    writeln!(body, "{name} {value}").unwrap();
}

fn write_helped_gauge<T: std::fmt::Display>(body: &mut String, name: &str, help: &str, value: T) {
    writeln!(body, "# HELP {name} {help}").unwrap();
    write_gauge(body, name, value);
}

fn write_counter<T: std::fmt::Display>(body: &mut String, name: &str, value: T) {
    writeln!(body, "{name} {value}").unwrap();
}

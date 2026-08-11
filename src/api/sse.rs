//! Shared response hardening for sensitive server-sent event streams.

use axum::http::{HeaderName, HeaderValue, header};
use axum::response::Response;

pub(super) fn hardened_response(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-transform"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

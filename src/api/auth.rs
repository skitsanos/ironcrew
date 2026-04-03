use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Bearer token authentication middleware.
///
/// Authentication priority (highest first):
/// 1. `IRONCREW_API_TOKEN` — static token, checked locally
/// 2. (Future) Remote token validation service
///
/// When no auth is configured, all requests pass through.
pub async fn bearer_auth(request: Request, next: Next) -> Response {
    // Priority 1: Static token from env var
    let expected_token = std::env::var("IRONCREW_API_TOKEN").ok();

    // No auth configured — pass through
    let Some(expected) = expected_token else {
        return next.run(request).await;
    };

    if expected.trim().is_empty() {
        return next.run(request).await;
    }

    // Extract Authorization header
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok());

    let Some(header_value) = auth_header else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "Missing Authorization header"})),
        )
            .into_response();
    };

    // Expect "Bearer <token>"
    let token = header_value.strip_prefix("Bearer ").unwrap_or(header_value);

    if token != expected {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "Invalid token"})),
        )
            .into_response();
    }

    next.run(request).await
}

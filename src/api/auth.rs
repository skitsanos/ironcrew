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
    let expected = match std::env::var("IRONCREW_API_TOKEN") {
        // No auth configured — pass through.
        Err(std::env::VarError::NotPresent) => return next.run(request).await,
        Ok(expected) if !expected.trim().is_empty() => expected,
        Ok(_) | Err(std::env::VarError::NotUnicode(_)) => {
            tracing::error!("IRONCREW_API_TOKEN is present but invalid; failing closed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "Authentication is misconfigured"})),
            )
                .into_response();
        }
    };

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

    // Require the Bearer scheme instead of accepting a raw token as the whole
    // header. Authentication schemes are case-insensitive; credentials are not.
    let Some((scheme, token)) = header_value.split_once(' ') else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "Invalid Authorization scheme"})),
        )
            .into_response();
    };
    if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "Invalid Authorization scheme"})),
        )
            .into_response();
    }

    // Hash both variable-length tokens, then compare the fixed-size digests
    // through ring's constant-time HMAC verifier. `ring::constant_time` is an
    // internal deprecated API; HMAC verification has the same fail-closed
    // comparison property without depending on it.
    let supplied_digest = ring::digest::digest(&ring::digest::SHA256, token.as_bytes());
    let expected_digest = ring::digest::digest(&ring::digest::SHA256, expected.as_bytes());
    let comparison_key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        b"ironcrew-api-token-constant-time-comparison",
    );
    let expected_tag = ring::hmac::sign(&comparison_key, expected_digest.as_ref());
    if ring::hmac::verify(
        &comparison_key,
        supplied_digest.as_ref(),
        expected_tag.as_ref(),
    )
    .is_err()
    {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": "Invalid token"})),
        )
            .into_response();
    }

    next.run(request).await
}

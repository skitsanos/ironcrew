use std::time::Duration;

use reqwest::{Client, RequestBuilder};
use serde_json::Value;

use super::{
    HttpToolPolicy, MAX_REQUEST_HEADERS, MAX_REQUEST_TIMEOUT_SECS, MAX_URL_BYTES,
    request_argument_error,
};
use crate::utils::error::Result;

pub(super) fn build(
    client: &Client,
    args: &Value,
    policy: &HttpToolPolicy,
) -> Result<RequestBuilder> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| request_argument_error("Missing 'url' argument"))?;
    if url.len() > MAX_URL_BYTES {
        return Err(request_argument_error(format!(
            "'url' exceeds the {MAX_URL_BYTES}-byte limit"
        )));
    }
    let method = args["method"]
        .as_str()
        .ok_or_else(|| request_argument_error("Missing or invalid 'method' argument"))?
        .to_uppercase();
    let mut request = match method.as_str() {
        "GET" => client.get(url),
        "POST" => client.post(url),
        "PUT" => client.put(url),
        "DELETE" => client.delete(url),
        "PATCH" => client.patch(url),
        other => {
            return Err(request_argument_error(format!(
                "Unsupported method: {other}"
            )));
        }
    };
    request = apply_timeout(request, args)?;
    let (next, header_bytes, header_count) = apply_headers(request, args, policy)?;
    request = apply_auth(next, args, policy, header_bytes, header_count)?;
    request = apply_body(request, args, policy)?;

    crate::utils::network::validate_url_with_private_access(
        url,
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
        policy.allow_private(),
    )
    .map_err(request_argument_error)?;
    Ok(request)
}

fn apply_timeout(mut request: RequestBuilder, args: &Value) -> Result<RequestBuilder> {
    let Some(value) = args.get("timeout_secs").filter(|value| !value.is_null()) else {
        return Ok(request);
    };
    let timeout = value
        .as_f64()
        .ok_or_else(|| request_argument_error("'timeout_secs' must be a number"))?;
    if !timeout.is_finite() || timeout <= 0.0 || timeout > MAX_REQUEST_TIMEOUT_SECS {
        return Err(request_argument_error(format!(
            "'timeout_secs' must be finite and greater than 0, up to {MAX_REQUEST_TIMEOUT_SECS} seconds"
        )));
    }
    request = request.timeout(Duration::from_secs_f64(timeout));
    Ok(request)
}

fn apply_headers(
    mut request: RequestBuilder,
    args: &Value,
    policy: &HttpToolPolicy,
) -> Result<(RequestBuilder, usize, usize)> {
    let Some(value) = args.get("headers").filter(|value| !value.is_null()) else {
        return Ok((request, 0, 0));
    };
    let headers = value
        .as_object()
        .ok_or_else(|| request_argument_error("'headers' must be an object"))?;
    if headers.len() > MAX_REQUEST_HEADERS {
        return Err(request_argument_error(format!(
            "'headers' contains more than {MAX_REQUEST_HEADERS} entries"
        )));
    }
    let mut bytes = 0usize;
    for (key, value) in headers {
        let value = value.as_str().ok_or_else(|| {
            request_argument_error(format!("header '{key}' value must be a string"))
        })?;
        bytes = add_header_bytes(bytes, key.len(), value.len(), policy)?;
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| request_argument_error(format!("invalid request header name '{key}'")))?;
        let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
            request_argument_error(format!("invalid value for request header '{key}'"))
        })?;
        request = request.header(name, value);
    }
    Ok((request, bytes, headers.len()))
}

fn apply_auth(
    mut request: RequestBuilder,
    args: &Value,
    policy: &HttpToolPolicy,
    bytes: usize,
    count: usize,
) -> Result<RequestBuilder> {
    let Some(value) = args.get("auth_type").filter(|value| !value.is_null()) else {
        return Ok(request);
    };
    let auth_type = value
        .as_str()
        .ok_or_else(|| request_argument_error("'auth_type' must be a string"))?;
    if count >= MAX_REQUEST_HEADERS {
        return Err(request_argument_error(format!(
            "request would exceed the {MAX_REQUEST_HEADERS}-header limit"
        )));
    }
    match auth_type {
        "bearer" => {
            let token = required_token(args, "bearer")?;
            let _ = add_header_bytes(
                bytes,
                "Authorization".len() + "Bearer ".len(),
                token.len(),
                policy,
            )?;
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        "basic" => {
            let username = optional_string(args, "auth_username", "")?;
            let password = optional_string(args, "auth_token", "")?;
            let estimate = username
                .len()
                .checked_add(password.len())
                .and_then(|total| total.checked_add("AuthorizationBasic ".len()))
                .and_then(|total| total.checked_mul(2))
                .ok_or_else(|| request_argument_error("request headers are too large"))?;
            let _ = add_header_bytes(bytes, estimate, 0, policy)?;
            request = request.basic_auth(username, Some(password));
        }
        "api_key" => {
            let header = optional_string(args, "auth_header", "X-API-Key")?;
            let key = required_token(args, "api_key")?;
            let _ = add_header_bytes(bytes, header.len(), key.len(), policy)?;
            let header = reqwest::header::HeaderName::from_bytes(header.as_bytes())
                .map_err(|_| request_argument_error("invalid API-key header name"))?;
            let key = reqwest::header::HeaderValue::from_str(key)
                .map_err(|_| request_argument_error("invalid API-key header value"))?;
            request = request.header(header, key);
        }
        other => {
            return Err(request_argument_error(format!(
                "unsupported auth_type '{other}'"
            )));
        }
    }
    Ok(request)
}

fn apply_body(
    mut request: RequestBuilder,
    args: &Value,
    policy: &HttpToolPolicy,
) -> Result<RequestBuilder> {
    let Some(value) = args.get("body").filter(|value| !value.is_null()) else {
        return Ok(request);
    };
    let body = value
        .as_str()
        .ok_or_else(|| request_argument_error("'body' must be a string"))?;
    if body.len() > policy.request_body_bytes() {
        return Err(request_argument_error(format!(
            "request body exceeds IRONCREW_HTTP_MAX_REQUEST_BODY_BYTES ({})",
            policy.request_body_bytes()
        )));
    }
    if body.starts_with('{') || body.starts_with('[') {
        request = request.header("Content-Type", "application/json");
    }
    Ok(request.body(body.to_string()))
}

fn add_header_bytes(
    current: usize,
    name: usize,
    value: usize,
    policy: &HttpToolPolicy,
) -> Result<usize> {
    let total = current
        .checked_add(name)
        .and_then(|total| total.checked_add(value))
        .ok_or_else(|| request_argument_error("request headers are too large"))?;
    if total > policy.request_header_bytes() {
        return Err(request_argument_error(format!(
            "request headers exceed IRONCREW_HTTP_MAX_REQUEST_HEADER_BYTES ({})",
            policy.request_header_bytes()
        )));
    }
    Ok(total)
}

fn required_token<'a>(args: &'a Value, auth_type: &str) -> Result<&'a str> {
    args["auth_token"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            request_argument_error(format!(
                "'auth_token' must be a non-empty string for {auth_type} auth"
            ))
        })
}

fn optional_string<'a>(args: &'a Value, key: &str, default: &'a str) -> Result<&'a str> {
    match args.get(key) {
        Some(value) if !value.is_null() => value
            .as_str()
            .ok_or_else(|| request_argument_error(format!("'{key}' must be a string"))),
        _ => Ok(default),
    }
}

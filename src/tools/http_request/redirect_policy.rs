//! Redirect handling for credential-bearing `http_request` calls.
//!
//! reqwest strips only `Authorization`/`Cookie` on a cross-host redirect, so a
//! request carrying a secret in a custom header needs a client that refuses to
//! leave the original origin entirely.

use std::sync::LazyLock;
use std::time::Duration;

use reqwest::Client;

use super::HttpToolPolicy;

/// Client variants that refuse cross-origin redirects. reqwest only strips
/// `Authorization`/`Cookie` across hosts, so a request carrying a secret in a
/// custom header (an `api_key` header, or caller-supplied headers) must not be
/// allowed to follow a 3xx to another origin and hand the secret over.
pub(super) fn build_same_origin_client(allow_private: bool) -> Client {
    crate::utils::network::secure_client_builder_with_private_access(
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
        allow_private,
    )
    .redirect(crate::utils::network::same_origin_redirect_policy(
        crate::utils::network::OutboundNetworkPolicy::PublicOnly,
        allow_private,
    ))
    .timeout(Duration::from_secs(30))
    .user_agent(format!("IronCrew/{}", env!("CARGO_PKG_VERSION")))
    .pool_max_idle_per_host(10)
    .build()
    .expect("Failed to build HTTP client")
}

static PUBLIC_SAME_ORIGIN_CLIENT: LazyLock<Client> =
    LazyLock::new(|| build_same_origin_client(false));
static PRIVATE_SAME_ORIGIN_CLIENT: LazyLock<Client> =
    LazyLock::new(|| build_same_origin_client(true));

/// True when the request would attach a credential or caller-controlled header
/// that reqwest would carry across a redirect.
pub(super) fn carries_credentials(args: &serde_json::Value) -> bool {
    let has_auth = args
        .get("auth_type")
        .and_then(|value| value.as_str())
        .is_some_and(|auth_type| !auth_type.is_empty() && auth_type != "none");
    let has_custom_headers = args
        .get("headers")
        .and_then(|value| value.as_object())
        .is_some_and(|headers| !headers.is_empty());
    has_auth || has_custom_headers
}

/// Pick the client for one request: credential-bearing requests get the
/// same-origin redirect policy, everything else keeps the normal SSRF policy.
pub(super) fn client_for_request(
    policy: &HttpToolPolicy,
    args: &serde_json::Value,
    base: &Client,
) -> Client {
    if !carries_credentials(args) {
        return base.clone();
    }
    if policy.allow_private() {
        PRIVATE_SAME_ORIGIN_CLIENT.clone()
    } else {
        PUBLIC_SAME_ORIGIN_CLIENT.clone()
    }
}

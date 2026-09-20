use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;

pub(super) fn source_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trust_proxy: bool,
) -> Option<String> {
    // A trusted append-style proxy writes the address it observed at the
    // right edge. Values to its left may have been supplied by the client.
    if trust_proxy && let Some(ip) = rightmost_ip(headers) {
        return Some(ip.to_string());
    }
    peer.map(|address| address.ip().to_string())
}

pub(super) fn rightmost_ip(headers: &HeaderMap) -> Option<IpAddr> {
    // Multiple header fields are legal. Treat the last field and its last
    // comma-separated entry as the trusted proxy's append position.
    let mut last_value = None;
    for value in headers.get_all("x-forwarded-for") {
        last_value = Some(value);
    }
    let candidate = last_value?.to_str().ok()?.rsplit(',').next()?.trim();
    candidate.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_proxy_uses_rightmost_forwarded_ip_not_forged_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "192.0.2.44, 203.0.113.9".parse().unwrap(),
        );
        let peer = Some("10.0.0.5:443".parse().unwrap());

        assert_eq!(
            source_ip(&headers, peer, true).as_deref(),
            Some("203.0.113.9")
        );
        assert_eq!(
            source_ip(&headers, peer, false).as_deref(),
            Some("10.0.0.5")
        );
    }

    #[test]
    fn trusted_proxy_falls_back_to_peer_when_rightmost_entry_is_invalid() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "192.0.2.44, not-an-ip".parse().unwrap());
        let peer = Some("10.0.0.5:443".parse().unwrap());

        assert_eq!(source_ip(&headers, peer, true).as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn trusted_proxy_uses_the_last_repeated_header_field() {
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", "192.0.2.44".parse().unwrap());
        headers.append("x-forwarded-for", "203.0.113.9".parse().unwrap());

        assert_eq!(
            source_ip(&headers, None, true).as_deref(),
            Some("203.0.113.9")
        );
    }
}

use std::net::{IpAddr, ToSocketAddrs};

/// Check if a URL targets a private/internal network address (SSRF protection).
/// Returns Ok(()) if the URL is safe, Err(reason) if it should be blocked.
///
/// Blocked ranges: loopback, link-local, RFC1918 private, multicast, broadcast.
/// Can be disabled via `IRONCREW_ALLOW_PRIVATE_IPS=1`.
pub fn validate_url_not_private(url: &str) -> Result<(), String> {
    if std::env::var("IRONCREW_ALLOW_PRIVATE_IPS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return Ok(());
    }

    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL: {}", e))?;

    let host = parsed.host_str().ok_or("URL has no host")?;

    // Try to parse as IP directly
    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_ip(ip);
    }

    // Resolve hostname to IP(s) and check all of them
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr_str = format!("{}:{}", host, port);
    if let Ok(addrs) = addr_str.to_socket_addrs() {
        for addr in addrs {
            check_ip(addr.ip())?;
        }
    }

    Ok(())
}

fn check_ip(ip: IpAddr) -> Result<(), String> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64)
            // CGNAT
            {
                return Err(format!(
                    "Blocked: request to private/internal IP {} is not allowed",
                    v4
                ));
            }
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            // Unique-local (fc00::/7) and link-local unicast (fe80::/10) are
            // the IPv6 equivalents of RFC1918 / 169.254.0.0 and must be blocked
            // alongside loopback (::1) and the unspecified address (::).
            let is_unique_local = (seg[0] & 0xfe00) == 0xfc00;
            let is_link_local = (seg[0] & 0xffc0) == 0xfe80;
            if v6.is_loopback() || v6.is_unspecified() || is_unique_local || is_link_local {
                return Err(format!(
                    "Blocked: request to private/internal IP {} is not allowed",
                    v6
                ));
            }
            // Unwrap embedded IPv4 so 4in6 forms can't smuggle a private v4
            // address past the checks above (::ffff:x.x.x.x, NAT64 64:ff9b::/96).
            if let Some(v4) = v6.to_ipv4_mapped() {
                return check_ip(IpAddr::V4(v4));
            }
            if seg[0] == 0x0064 && seg[1] == 0xff9b {
                let v4 = std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return check_ip(IpAddr::V4(v4));
            }
        }
    }
    Ok(())
}

/// Build a redirect policy that re-validates every hop against the SSRF filter.
///
/// The plain `validate_url_not_private` check only covers the URL a caller
/// passes in; without this, a public URL that responds with a 3xx to
/// `http://169.254.169.254/…` (or any private address) would still be followed
/// by reqwest's default policy. This closes that bypass and caps the chain at
/// 10 hops. Attach it via `ClientBuilder::redirect`.
pub fn ssrf_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects (max 10)".to_string());
        }
        match validate_url_not_private(attempt.url().as_str()) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    #[test]
    fn blocks_private_and_internal_v4() {
        for ip in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1"] {
            assert!(check_ip(v4(ip)).is_err(), "expected {ip} to be blocked");
        }
    }

    #[test]
    fn allows_public_v4() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            assert!(check_ip(v4(ip)).is_ok(), "expected {ip} to be allowed");
        }
    }

    #[test]
    fn blocks_private_v6_ranges() {
        // loopback, unspecified, unique-local (fc00::/7), link-local (fe80::/10)
        for ip in ["::1", "::", "fc00::1", "fd12:3456::1", "fe80::1"] {
            assert!(check_ip(v6(ip)).is_err(), "expected {ip} to be blocked");
        }
    }

    #[test]
    fn blocks_embedded_private_v4_in_v6() {
        // IPv4-mapped and NAT64 wrappers must not smuggle a private v4 through.
        assert!(check_ip(v6("::ffff:127.0.0.1")).is_err());
        assert!(check_ip(v6("64:ff9b::a00:1")).is_err()); // 64:ff9b::10.0.0.1
    }

    #[test]
    fn allows_public_v6() {
        assert!(check_ip(v6("2606:4700:4700::1111")).is_ok());
    }
}

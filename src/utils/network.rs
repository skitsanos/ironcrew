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
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64) // CGNAT
            {
                return Err(format!(
                    "Blocked: request to private/internal IP {} is not allowed",
                    v4
                ));
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return Err(format!(
                    "Blocked: request to private/internal IP {} is not allowed",
                    v6
                ));
            }
            // Check IPv4-mapped IPv6 (::ffff:x.x.x.x)
            if let Some(v4) = v6.to_ipv4_mapped() {
                return check_ip(IpAddr::V4(v4));
            }
        }
    }
    Ok(())
}

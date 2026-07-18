use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Network destinations an outbound client may connect to.
///
/// `AllowLoopback` exists for explicitly configured local MCP servers. It does
/// not permit RFC1918, link-local, CGNAT, multicast, documentation, or other
/// special-use destinations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OutboundNetworkPolicy {
    #[default]
    PublicOnly,
    AllowLoopback,
}

fn private_ips_override_enabled() -> bool {
    std::env::var("IRONCREW_ALLOW_PRIVATE_IPS")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Check whether a URL resolves only to public, globally routable addresses.
///
/// This synchronous preflight validates syntax and literal IP destinations.
/// Hostname resolution is intentionally deferred to [`SafeDnsResolver`], which
/// runs asynchronously and applies the policy to the addresses used by the
/// actual connection, closing the DNS-rebinding/resolve-then-connect gap
/// without blocking a Tokio worker on the system resolver.
pub fn validate_url_not_private(url: &str) -> Result<(), String> {
    validate_url_with_policy(url, OutboundNetworkPolicy::PublicOnly)
}

pub fn validate_url_with_policy(url: &str, policy: OutboundNetworkPolicy) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("Invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "Unsupported URL scheme '{}': only http and https are allowed",
            parsed.scheme()
        ));
    }

    let host = parsed.host_str().ok_or("URL has no host")?;
    if private_ips_override_enabled() {
        return Ok(());
    }
    if host.trim_end_matches('.').eq_ignore_ascii_case("localhost") {
        return check_ip(IpAddr::V4(Ipv4Addr::LOCALHOST), policy);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_ip(ip, policy);
    }

    Ok(())
}

fn ipv4_in_prefix(address: Ipv4Addr, network: Ipv4Addr, prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (u32::from(address) & mask) == (u32::from(network) & mask)
}

fn ipv6_in_prefix(address: Ipv6Addr, network: Ipv6Addr, prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    (u128::from(address) & mask) == (u128::from(network) & mask)
}

fn check_ip(ip: IpAddr, policy: OutboundNetworkPolicy) -> Result<(), String> {
    if policy == OutboundNetworkPolicy::AllowLoopback && ip.is_loopback() {
        return Ok(());
    }

    let blocked = match ip {
        IpAddr::V4(v4) => {
            // IANA IPv4 special-purpose ranges that must never be reachable by
            // an untrusted outbound request. Keep this explicit instead of
            // relying on unstable `is_global` APIs.
            [
                (Ipv4Addr::new(0, 0, 0, 0), 8),       // current network
                (Ipv4Addr::new(10, 0, 0, 0), 8),      // RFC1918
                (Ipv4Addr::new(100, 64, 0, 0), 10),   // shared/CGNAT
                (Ipv4Addr::new(127, 0, 0, 0), 8),     // loopback
                (Ipv4Addr::new(169, 254, 0, 0), 16),  // link-local
                (Ipv4Addr::new(172, 16, 0, 0), 12),   // RFC1918
                (Ipv4Addr::new(192, 0, 0, 0), 24),    // IETF protocols
                (Ipv4Addr::new(192, 0, 2, 0), 24),    // documentation
                (Ipv4Addr::new(192, 88, 99, 0), 24),  // deprecated 6to4 relay
                (Ipv4Addr::new(192, 168, 0, 0), 16),  // RFC1918
                (Ipv4Addr::new(198, 18, 0, 0), 15),   // benchmarking
                (Ipv4Addr::new(198, 51, 100, 0), 24), // documentation
                (Ipv4Addr::new(203, 0, 113, 0), 24),  // documentation
                (Ipv4Addr::new(224, 0, 0, 0), 4),     // multicast
                (Ipv4Addr::new(240, 0, 0, 0), 4),     // reserved/broadcast
            ]
            .into_iter()
            .any(|(network, prefix)| ipv4_in_prefix(v4, network, prefix))
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped and both standardized NAT64 prefixes must be
            // unwrapped so a private IPv4 target cannot be smuggled in IPv6.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return check_ip(IpAddr::V4(v4), policy);
            }
            if ipv6_in_prefix(v6, "64:ff9b::".parse().expect("valid prefix"), 96) {
                let octets = v6.octets();
                return check_ip(
                    IpAddr::V4(Ipv4Addr::new(
                        octets[12], octets[13], octets[14], octets[15],
                    )),
                    policy,
                );
            }

            [
                ("::".parse().expect("valid prefix"), 96), // IPv4-compatible/special
                ("::1".parse().expect("valid prefix"), 128), // loopback
                ("64:ff9b:1::".parse().expect("valid prefix"), 48), // local NAT64
                ("100::".parse().expect("valid prefix"), 64), // discard-only
                ("2001::".parse().expect("valid prefix"), 32), // Teredo
                ("2001:2::".parse().expect("valid prefix"), 48), // benchmark
                ("2001:10::".parse().expect("valid prefix"), 28), // ORCHID
                ("2001:20::".parse().expect("valid prefix"), 28), // ORCHIDv2
                ("2001:db8::".parse().expect("valid prefix"), 32), // docs
                ("3fff::".parse().expect("valid prefix"), 20), // docs
                ("5f00::".parse().expect("valid prefix"), 16), // segment-routing SIDs
                ("fc00::".parse().expect("valid prefix"), 7), // unique-local
                ("fe80::".parse().expect("valid prefix"), 10), // link-local
                ("fec0::".parse().expect("valid prefix"), 10), // site-local
                ("ff00::".parse().expect("valid prefix"), 8), // multicast
            ]
            .into_iter()
            .any(|(network, prefix)| ipv6_in_prefix(v6, network, prefix))
        }
    };

    if blocked {
        Err(format!(
            "Blocked: request to private/internal IP {ip} is not allowed"
        ))
    } else {
        Ok(())
    }
}

/// Resolver used by outbound clients to validate the address that reqwest will
/// actually connect to. A DNS error, empty answer, or a single non-public
/// answer rejects the entire resolution; there is no fail-open path.
#[derive(Clone, Copy, Debug)]
pub struct SafeDnsResolver {
    policy: OutboundNetworkPolicy,
}

impl SafeDnsResolver {
    pub fn new(policy: OutboundNetworkPolicy) -> Self {
        Self { policy }
    }
}

impl Default for SafeDnsResolver {
    fn default() -> Self {
        Self::new(OutboundNetworkPolicy::PublicOnly)
    }
}

impl Resolve for SafeDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let hostname = name.as_str().to_owned();
        let policy = self.policy;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((hostname.as_str(), 0))
                .await
                .map_err(|error| {
                    Box::new(std::io::Error::other(format!(
                        "DNS resolution failed for '{hostname}': {error}"
                    ))) as Box<dyn std::error::Error + Send + Sync>
                })?
                .collect::<Vec<SocketAddr>>();

            if addresses.is_empty() {
                return Err(Box::new(std::io::Error::other(format!(
                    "DNS resolution returned no addresses for '{hostname}'"
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }

            if !private_ips_override_enabled() {
                // Loopback is only intentional for the literal loopback forms
                // or the conventional localhost name. Do not let an arbitrary
                // public-looking hostname rebind to 127/8 or ::1.
                let loopback_name = hostname
                    .trim_end_matches('.')
                    .eq_ignore_ascii_case("localhost")
                    || hostname
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback());
                let resolution_policy =
                    if policy == OutboundNetworkPolicy::AllowLoopback && loopback_name {
                        policy
                    } else {
                        OutboundNetworkPolicy::PublicOnly
                    };
                for address in &addresses {
                    check_ip(address.ip(), resolution_policy).map_err(|reason| {
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            reason,
                        )) as Box<dyn std::error::Error + Send + Sync>
                    })?;
                }
            }

            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// Start a reqwest client builder whose actual DNS resolution is SSRF-safe.
///
/// Environment proxy discovery is disabled because an HTTP proxy would resolve
/// the destination itself, bypassing the in-process resolver. Callers can still
/// opt into private destinations explicitly with `IRONCREW_ALLOW_PRIVATE_IPS`.
pub fn secure_client_builder(policy: OutboundNetworkPolicy) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(SafeDnsResolver::new(policy))
        .redirect(ssrf_redirect_policy_with_policy(policy))
}

/// Build an SSRF-aware client with redirects disabled. This is useful for
/// protocols such as MCP where redirect behavior belongs to the transport.
pub fn secure_no_redirect_client(
    policy: OutboundNetworkPolicy,
) -> Result<reqwest::Client, reqwest::Error> {
    secure_client_builder(policy)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

pub fn ssrf_redirect_policy_with_policy(
    policy: OutboundNetworkPolicy,
) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects (max 10)".to_string());
        }
        match validate_url_with_policy(attempt.url().as_str(), policy) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(value: &str) -> IpAddr {
        IpAddr::V4(value.parse().expect("valid test IPv4"))
    }

    fn v6(value: &str) -> IpAddr {
        IpAddr::V6(value.parse().expect("valid test IPv6"))
    }

    #[test]
    fn blocks_private_reserved_and_documentation_ipv4() {
        for ip in [
            "0.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(
                check_ip(v4(ip), OutboundNetworkPolicy::PublicOnly).is_err(),
                "expected {ip} to be blocked"
            );
        }
    }

    #[test]
    fn allows_public_ipv4() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            assert!(
                check_ip(v4(ip), OutboundNetworkPolicy::PublicOnly).is_ok(),
                "expected {ip} to be allowed"
            );
        }
    }

    #[test]
    fn blocks_private_reserved_and_documentation_ipv6() {
        for ip in [
            "::1",
            "::",
            "64:ff9b:1::1",
            "100::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "3fff::1",
            "5f00::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        ] {
            assert!(
                check_ip(v6(ip), OutboundNetworkPolicy::PublicOnly).is_err(),
                "expected {ip} to be blocked"
            );
        }
    }

    #[test]
    fn blocks_embedded_private_ipv4_but_allows_public_nat64() {
        assert!(check_ip(v6("::ffff:127.0.0.1"), OutboundNetworkPolicy::PublicOnly).is_err());
        assert!(check_ip(v6("64:ff9b::a00:1"), OutboundNetworkPolicy::PublicOnly).is_err());
        assert!(check_ip(v6("64:ff9b::808:808"), OutboundNetworkPolicy::PublicOnly).is_ok());
    }

    #[test]
    fn loopback_only_policy_does_not_allow_other_private_ranges() {
        assert!(check_ip(v4("127.0.0.1"), OutboundNetworkPolicy::AllowLoopback).is_ok());
        assert!(check_ip(v6("::1"), OutboundNetworkPolicy::AllowLoopback).is_ok());
        assert!(check_ip(v4("10.0.0.1"), OutboundNetworkPolicy::AllowLoopback).is_err());
        assert!(check_ip(v6("fd00::1"), OutboundNetworkPolicy::AllowLoopback).is_err());
    }

    #[test]
    fn rejects_bad_scheme_and_defers_hostname_resolution() {
        assert!(validate_url_not_private("file:///etc/passwd").is_err());
        assert!(validate_url_not_private("http://definitely-not-a-real-host.invalid/").is_ok());
    }

    #[test]
    fn redirect_targets_are_validated_with_the_selected_policy() {
        assert!(
            validate_url_with_policy(
                "http://127.0.0.1/metadata",
                OutboundNetworkPolicy::PublicOnly
            )
            .is_err()
        );
        assert!(
            validate_url_with_policy(
                "http://127.0.0.1/local-mcp",
                OutboundNetworkPolicy::AllowLoopback
            )
            .is_ok()
        );
    }

    #[test]
    fn allows_public_ipv6() {
        assert!(
            check_ip(
                v6("2606:4700:4700::1111"),
                OutboundNetworkPolicy::PublicOnly
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn connection_resolver_fails_closed_for_private_and_missing_names() {
        let private = SafeDnsResolver::default()
            .resolve("localhost".parse().expect("valid DNS name"))
            .await;
        assert!(
            private.is_err(),
            "public-only resolver must reject loopback"
        );

        let missing = SafeDnsResolver::default()
            .resolve(
                "definitely-not-a-real-host.invalid"
                    .parse()
                    .expect("valid DNS name"),
            )
            .await;
        assert!(missing.is_err(), "DNS failures must not fail open");
    }
}

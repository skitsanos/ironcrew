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

pub(crate) fn private_ips_override_enabled() -> bool {
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
    validate_url_with_private_access(url, policy, private_ips_override_enabled())
}

pub(crate) fn validate_url_with_private_access(
    url: &str,
    policy: OutboundNetworkPolicy,
    allow_private: bool,
) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("Invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "Unsupported URL scheme '{}': only http and https are allowed",
            parsed.scheme()
        ));
    }

    let host = parsed.host_str().ok_or("URL has no host")?;
    if allow_private {
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
    allow_private: bool,
}

impl SafeDnsResolver {
    pub fn new(policy: OutboundNetworkPolicy) -> Self {
        Self::with_private_access(policy, private_ips_override_enabled())
    }

    pub(crate) fn with_private_access(policy: OutboundNetworkPolicy, allow_private: bool) -> Self {
        Self {
            policy,
            allow_private,
        }
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
        let allow_private = self.allow_private;
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

            if !allow_private {
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
    secure_client_builder_with_private_access(policy, private_ips_override_enabled())
}

pub(crate) fn secure_client_builder_with_private_access(
    policy: OutboundNetworkPolicy,
    allow_private: bool,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(SafeDnsResolver::with_private_access(policy, allow_private))
        .redirect(ssrf_redirect_policy_with_private_access(
            policy,
            allow_private,
        ))
}

/// Build an SSRF-aware client with redirects disabled. This is useful for
/// protocols such as MCP where redirect behavior belongs to the transport.
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub fn secure_no_redirect_client(
    policy: OutboundNetworkPolicy,
) -> Result<reqwest::Client, reqwest::Error> {
    secure_client_builder(policy)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// SSRF-aware redirect policy that additionally refuses to leave the original
/// origin.
///
/// reqwest strips only `Authorization`/`Cookie`-style headers across hosts, so
/// a request carrying a secret in a custom header (`x-api-key`, a configured
/// `api_key` header) would hand that secret to whatever public host a 3xx
/// pointed at. Callers that send such headers use this policy so a redirect can
/// never move the credential to another origin.
pub(crate) fn same_origin_redirect_policy(
    policy: OutboundNetworkPolicy,
    allow_private: bool,
) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects (max 10)".to_string());
        }
        let Some(origin) = attempt.previous().first() else {
            return attempt.error("redirect without an originating URL".to_string());
        };
        let target = attempt.url();
        let same_origin = target.scheme() == origin.scheme()
            && target.host_str() == origin.host_str()
            && target.port_or_known_default() == origin.port_or_known_default();
        if !same_origin {
            return attempt.error(
                "refusing to follow a cross-origin redirect on a request carrying credentials"
                    .to_string(),
            );
        }
        match validate_url_with_private_access(target.as_str(), policy, allow_private) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    })
}

#[allow(dead_code)] // public helper retained for embedders; built-ins use a captured override
pub fn ssrf_redirect_policy_with_policy(
    policy: OutboundNetworkPolicy,
) -> reqwest::redirect::Policy {
    ssrf_redirect_policy_with_private_access(policy, private_ips_override_enabled())
}

fn ssrf_redirect_policy_with_private_access(
    policy: OutboundNetworkPolicy,
    allow_private: bool,
) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects (max 10)".to_string());
        }
        match validate_url_with_private_access(attempt.url().as_str(), policy, allow_private) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    })
}

#[cfg(test)]
mod tests;

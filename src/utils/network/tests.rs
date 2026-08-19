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

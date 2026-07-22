//! Resolve the real client IP for a request, honoring `X-Forwarded-For` only
//! when the direct peer is a configured trusted proxy.
//!
//! `X-Forwarded-For` is a client-forgeable header, so it is trusted selectively:
//! only when the TCP peer that opened the connection to the agora is itself in
//! the operator-configured trusted-proxy set. The default trusted set is
//! loopback, matching the default same-box reverse-proxy deploy.

use std::net::IpAddr;

use axum::http::HeaderMap;
use ipnet::IpNet;

/// Resolve the real client IP behind an (optional) trusted proxy chain.
///
/// - If `peer` is NOT in any trusted net, XFF is never trusted: return `peer`
///   (a direct client, or an untrusted proxy whose XFF may be forged).
/// - If `peer` is trusted, parse `X-Forwarded-For` left-to-right as
///   `client, proxy1, proxy2, ...`. Scan right-to-left and return the first hop
///   not in a trusted net (the real client as the last untrusted hop).
/// - If XFF is absent/empty while the peer is trusted, return `peer`: the proxy
///   did not append XFF, so granularity degrades to the proxy IP. Operators must
///   ensure their proxy appends XFF.
/// - If every XFF hop is itself trusted (a fully-trusted chain), return the
///   leftmost entry: the originating client's claimed IP. This is only
///   spoofable by a trusted peer, which is the threat model.
pub fn real_client_ip(headers: &HeaderMap, peer: IpAddr, trusted: &[IpNet]) -> IpAddr {
    if !trusted.iter().any(|net| net.contains(&peer)) {
        return peer;
    }
    let Some(hops) = parse_xff(headers) else {
        return peer;
    };
    // Rightmost non-trusted hop = the real client.
    for hop in hops.iter().rev() {
        if !trusted.iter().any(|net| net.contains(hop)) {
            return *hop;
        }
    }
    // All hops trusted: take the originating (leftmost) claim.
    hops[0]
}

/// Parse `X-Forwarded-For` into client IPs, left-to-right, skipping unparseable
/// entries. Returns `None` when the header is absent or contains no valid IP.
fn parse_xff(headers: &HeaderMap) -> Option<Vec<IpAddr>> {
    let mut out = Vec::new();
    for value in headers.get_all("x-forwarded-for") {
        let Ok(s) = value.to_str() else { continue };
        for part in s.split(',') {
            let trimmed = part.trim();
            if let Ok(ip) = trimmed.parse::<IpAddr>() {
                out.push(ip);
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    fn trusted_loopback() -> Vec<IpNet> {
        vec![
            IpNet::from_str("127.0.0.0/8").unwrap(),
            IpNet::from_str("::1/128").unwrap(),
        ]
    }

    fn headers_with_xff(xff: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_str(xff).unwrap());
        h
    }

    #[test]
    fn direct_client_untrusted_peer_ignores_xff() {
        // Peer is a public IP, not trusted. A forged XFF must be ignored.
        let peer = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        let headers = headers_with_xff("10.0.0.1");
        assert_eq!(real_client_ip(&headers, peer, &trusted_loopback()), peer);
    }

    #[test]
    fn trusted_proxy_rightmost_untrusted_hop_wins() {
        let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let headers = headers_with_xff("198.51.100.7");
        let got = real_client_ip(&headers, peer, &trusted_loopback());
        assert_eq!(got, IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
    }

    #[test]
    fn chained_proxies_skip_trusted_hops() {
        // Two-tier proxy: client, then loopback proxy, then loopback peer.
        let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let headers = headers_with_xff("198.51.100.7, 127.0.0.2");
        let got = real_client_ip(&headers, peer, &trusted_loopback());
        assert_eq!(got, IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
    }

    #[test]
    fn all_trusted_chain_takes_leftmost() {
        let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let headers = headers_with_xff("127.0.0.5, 127.0.0.6");
        let got = real_client_ip(&headers, peer, &trusted_loopback());
        assert_eq!(got, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 5)));
    }

    #[test]
    fn injected_internal_ip_does_not_override_real_client() {
        // Attacker forges an XFF claiming to be loopback then the real client.
        // Rightmost-untrusted still resolves to the genuine client.
        let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let headers = headers_with_xff("127.0.0.1, 198.51.100.7, 10.0.0.99");
        let got = real_client_ip(&headers, peer, &trusted_loopback());
        // 10.0.0.99 is not trusted -> it is the last untrusted hop.
        assert_eq!(got, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99)));
    }

    #[test]
    fn ipv6_peer_trusted_resolves_ipv6_client() {
        let peer = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let headers = headers_with_xff("2001:db8::1");
        let got = real_client_ip(&headers, peer, &trusted_loopback());
        assert_eq!(got, IpAddr::V6(Ipv6Addr::from_str("2001:db8::1").unwrap()));
    }

    #[test]
    fn trusted_peer_without_xff_falls_back_to_peer() {
        let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let headers = HeaderMap::new();
        assert_eq!(real_client_ip(&headers, peer, &trusted_loopback()), peer);
    }

    #[test]
    fn empty_trusted_set_never_trusts_xff() {
        let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let headers = headers_with_xff("198.51.100.7");
        assert_eq!(real_client_ip(&headers, peer, &[]), peer);
    }

    #[test]
    fn trusted_peer_with_unparseable_xff_falls_back_to_peer() {
        // A forged/garbage XFF degrades to the peer IP rather than panicking.
        let peer = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let headers = headers_with_xff("not-an-ip, ???");
        assert_eq!(real_client_ip(&headers, peer, &trusted_loopback()), peer);
    }
}

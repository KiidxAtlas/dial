//! Network egress guard: which addresses a model-supplied URL may connect to.
//!
//! The check runs inside the HTTP client's name resolution ([`GuardedResolver`]), so the addresses
//! that were validated are exactly the ones the client connects to; there is no separate lookup an
//! attacker's DNS could answer differently (rebinding). Redirects are followed by the caller one hop
//! at a time, so every hop passes through the resolver again.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use ureq::Error;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::NextTimeout;

/// Why connecting to `ip` is refused, or `None` when it is a public, routable address.
///
/// Covers loopback, unspecified, private (RFC 1918, ULA), link-local (including the cloud metadata
/// address 169.254.169.254), shared/CGNAT, benchmarking, documentation, multicast and reserved
/// ranges. IPv6 forms that embed an IPv4 address (mapped, NAT64, 6to4) are judged by the embedded
/// address; deprecated IPv4-compatible addresses are refused outright.
pub fn blocked_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => blocked_v6(v6),
    }
}

fn blocked_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, c, _] = ip.octets();
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if ip.is_link_local() {
        Some("link-local (cloud metadata)")
    } else if ip.is_private() {
        Some("private")
    } else if a == 100 && (64..=127).contains(&b) {
        Some("shared address space (CGNAT)")
    } else if a == 192 && b == 0 && c == 0 {
        Some("IETF protocol assignment")
    } else if a == 198 && (18..=19).contains(&b) {
        Some("benchmarking")
    } else if ip.is_documentation() {
        Some("documentation")
    } else if ip.is_multicast() {
        Some("multicast")
    } else if ip.is_broadcast() || a >= 240 {
        Some("reserved")
    } else if a == 0 {
        Some("unspecified")
    } else {
        None
    }
}

fn blocked_v6(ip: Ipv6Addr) -> Option<&'static str> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return blocked_v4(v4);
    }
    let seg = ip.segments();
    if ip.is_loopback() {
        Some("loopback")
    } else if ip.is_unspecified() {
        Some("unspecified")
    } else if ip.is_multicast() {
        Some("multicast")
    } else if seg[0] & 0xfe00 == 0xfc00 {
        Some("unique local (private)")
    } else if seg[0] & 0xffc0 == 0xfe80 {
        Some("link-local")
    } else if seg[0] & 0xffc0 == 0xfec0 {
        Some("site-local")
    } else if seg[..6] == [0, 0, 0, 0, 0, 0] {
        // IPv4-compatible (::a.b.c.d): deprecated, never a legitimate public destination.
        Some("IPv4-compatible")
    } else if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        // NAT64 well-known prefix: the embedded IPv4 address is what is actually reached.
        blocked_v4(Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        ))
    } else if seg[0] == 0x2002 {
        // 6to4: the IPv4 address sits in bits 16..48.
        blocked_v4(Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            seg[1] as u8,
            (seg[2] >> 8) as u8,
            seg[2] as u8,
        ))
    } else if seg[0] == 0x2001 && seg[1] == 0x0db8 {
        Some("documentation")
    } else {
        None
    }
}

/// Name resolution that refuses a host if ANY of its addresses is not publicly routable.
#[derive(Debug)]
pub struct GuardedResolver {
    /// Tests talk to an in-process server on 127.0.0.1; production never does.
    #[cfg(test)]
    allow_loopback: bool,
}

impl GuardedResolver {
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            allow_loopback: false,
        }
    }

    #[cfg(test)]
    pub fn allowing_loopback() -> Self {
        Self {
            allow_loopback: true,
        }
    }

    fn check(&self, ip: IpAddr) -> Option<&'static str> {
        #[cfg(test)]
        if self.allow_loopback && ip.is_loopback() {
            return None;
        }
        blocked_reason(ip)
    }
}

impl Resolver for GuardedResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let addrs = DefaultResolver::default().resolve(uri, config, timeout)?;
        for addr in addrs.iter() {
            if let Some(reason) = self.check(addr.ip()) {
                let host = uri.host().unwrap_or("?");
                let refusal = format!(
                    "refusing to connect to {host}: it resolves to a {reason} address ({})",
                    addr.ip()
                );
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    refusal,
                )));
            }
        }
        Ok(addrs)
    }
}

#[cfg(test)]
mod tests;

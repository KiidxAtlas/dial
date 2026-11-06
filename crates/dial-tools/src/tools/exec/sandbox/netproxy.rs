//! Domain-filtered network access for sandboxed commands (`sandbox.network_allow` and
//! `sandbox.network_deny`).
//!
//! When rules are configured and the policy allows network, the sandbox admits no direct network
//! at all: on macOS the profile allows only outbound TCP to this process's loopback proxy port; on
//! Linux the command runs in its own network namespace and reaches the proxy through a Unix
//! socket bound into the sandbox, bridged to a loopback port inside the namespace by a re-exec of
//! the Dial binary ([`BRIDGE_ARG`]). Clients find the proxy through the standard proxy variables
//! ([`client_env`]); a program that ignores them has no route out.
//!
//! The proxy speaks HTTP `CONNECT` (any TCP protocol, usually TLS) and absolute-form plain HTTP.
//! It resolves names itself, checks the host against the rules, refuses non-public addresses
//! (loopback, private, link-local and cloud metadata, carrier-grade NAT, multicast) unless the
//! host matched an exact allow entry, and then connects to the address it checked, so DNS
//! rebinding cannot swap in a different target. Denied requests get `403` with an explanation.
//!
//! Limits: rules match host names, not URLs or ports; DNS lookups happen outside the sandbox on
//! the proxy's resolver; UDP (including QUIC and direct DNS) has no route.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};

/// Private re-exec entry point that bridges a loopback port to the proxy socket inside the Linux
/// network namespace, then runs the command. It is not a user-facing command.
pub const BRIDGE_ARG: &str = "--dial-net-bridge";

/// Private bridge launch mode: install eval's process fence only in the command child.
pub(super) const FENCED_SEPARATOR: &str = "--eval-fenced";

/// Port the Linux bridge listens on inside the sandbox's private network namespace.
pub const BRIDGE_PORT: u16 = 3128;

/// Validated domain rules. Patterns are lowercase host names (`example.com`), subdomain wildcards
/// (`*.example.com`), IP literals, or `*` for any host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkRules {
    allow: Vec<String>,
    deny: Vec<String>,
}

/// What the proxy does with a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Deny,
    /// Allowed. `private_ok` is true only for an exact (non-wildcard) allow entry, which may
    /// resolve to a non-public address such as an internal package mirror.
    Allow {
        private_ok: bool,
    },
}

impl NetworkRules {
    /// Rules from configuration, or `None` when both lists are empty (unfiltered network).
    pub fn new(allow: &[String], deny: &[String]) -> Result<Option<Self>, String> {
        if allow.is_empty() && deny.is_empty() {
            return Ok(None);
        }
        let parse = |list: &[String], key: &str| {
            list.iter()
                .map(|pattern| {
                    normalize_pattern(pattern)
                        .map_err(|error| format!("sandbox.{key}: {pattern:?}: {error}"))
                })
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Some(Self {
            allow: parse(allow, "network_allow")?,
            deny: parse(deny, "network_deny")?,
        }))
    }

    /// Denials win; an empty allow list admits every host that is not denied.
    pub fn decide(&self, host: &str) -> Decision {
        let host = normalize_host(host);
        if self.deny.iter().any(|pattern| matches(pattern, &host)) {
            return Decision::Deny;
        }
        if self.allow.is_empty() {
            return Decision::Allow { private_ok: false };
        }
        if self.allow.iter().any(|pattern| pattern == &host) {
            return Decision::Allow { private_ok: true };
        }
        if self.allow.iter().any(|pattern| matches(pattern, &host)) {
            return Decision::Allow { private_ok: false };
        }
        Decision::Deny
    }
}

fn normalize_host(host: &str) -> String {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn normalize_pattern(pattern: &str) -> Result<String, &'static str> {
    let raw = pattern.trim();
    let pattern = normalize_host(raw);
    if pattern == "*" && raw == "*" {
        return Ok(pattern);
    }
    if pattern.parse::<IpAddr>().is_ok() {
        return Ok(pattern);
    }
    let name = pattern.strip_prefix("*.").unwrap_or(&pattern);
    let valid = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        });
    if valid {
        Ok(pattern)
    } else {
        Err("expected a host name, *.domain, an IP address or * (no scheme, port or path)")
    }
}

fn matches(pattern: &str, host: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_prefix("*.") {
        Some(domain) => host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
        None => pattern == host,
    }
}

/// True for globally routable unicast addresses. Everything a sandboxed command could use to
/// reach this machine, the local network or a cloud metadata service is not public.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let segments = v6.segments();
            // NAT64 (64:ff9b::/96) embeds an IPv4 target.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = segments[6].to_be_bytes();
                let [c, d] = segments[7].to_be_bytes();
                return is_public_v4(Ipv4Addr::new(a, b, c, d));
            }
            // 6to4 (2002::/16) embeds the IPv4 address of its relay endpoint.
            if segments[0] == 0x2002 {
                let [a, b] = segments[1].to_be_bytes();
                let [c, d] = segments[2].to_be_bytes();
                return is_public_v4(Ipv4Addr::new(a, b, c, d));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || segments[..6] == [0, 0, 0, 0, 0, 0])
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..20).contains(&b)))
}

/// Environment variables that point standard clients (curl, git, cargo, pip, npm and most HTTP
/// libraries) at the proxy on `port`. `NO_PROXY` is cleared so no host bypasses it.
pub fn client_env(port: u16) -> Vec<(&'static str, String)> {
    let url = format!("http://127.0.0.1:{port}");
    let mut env: Vec<(&'static str, String)> = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "npm_config_proxy",
        "npm_config_https_proxy",
    ]
    .into_iter()
    .map(|key| (key, url.clone()))
    .collect();
    env.push(("NO_PROXY", String::new()));
    env.push(("no_proxy", String::new()));
    env
}

#[cfg(unix)]
mod server;
#[cfg(unix)]
pub use server::{Endpoint, bridge_main, endpoint};

#[cfg(test)]
mod tests;

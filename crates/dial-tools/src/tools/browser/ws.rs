//! WebSocket policy for browser pages.
//!
//! Page WebSockets are routed through the driver, which connects only to the
//! endpoints listed here. The list is derived from the approved `allowed_hosts`
//! on every request, so the driver can never widen it. Only exact loopback
//! `host:port` entries qualify. Everything else is closed with 1008.

/// Loopback `host:port` entries from the approved allowlist, in order.
pub(super) fn websocket_endpoints(allowed_hosts: &[String]) -> Vec<String> {
    allowed_hosts
        .iter()
        .filter(|entry| {
            entry.rsplit_once(':').is_some_and(|(host, port)| {
                matches!(host, "localhost" | "127.0.0.1")
                    && port.parse::<u16>().is_ok_and(|p| p > 0)
            })
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests;

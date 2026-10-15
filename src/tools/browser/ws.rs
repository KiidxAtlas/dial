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
mod tests {
    use super::*;

    #[test]
    fn only_exact_loopback_approved_entries_become_websocket_endpoints() {
        let hosts: Vec<String> = [
            "localhost:4000",
            "127.0.0.1:3000",
            "10.0.0.5:8080",
            "example.com:443",
            "localhost:0",
            "localhost:notaport",
            "::1:3000",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            websocket_endpoints(&hosts),
            ["localhost:4000", "127.0.0.1:3000"]
        );
    }

    #[test]
    fn empty_allowlist_yields_no_endpoints() {
        assert!(websocket_endpoints(&[]).is_empty());
    }
}

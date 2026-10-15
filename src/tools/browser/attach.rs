//! Validation for attaching to an operator-started Chromium over CDP.
//!
//! Attach is opt-in and loopback-only: the endpoint must be `localhost`,
//! `127.0.0.1`, or `[::1]` with an explicit port and no credentials. Anything
//! else could point the driver at a remote browser that Dial did not start.

use url::{Host, Url};

/// Normalized CDP endpoint, or an error explaining why it was refused.
pub(super) fn validate_cdp_url(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw).map_err(|error| format!("browser.cdp_url is not a URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "ws") {
        return Err("browser.cdp_url must use http or ws".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("browser.cdp_url must not embed credentials".into());
    }
    let loopback = match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !loopback {
        return Err(
            "browser.cdp_url must be a loopback address (localhost, 127.0.0.1, or [::1])".into(),
        );
    }
    if url.port().is_none() {
        return Err("browser.cdp_url must include an explicit port".into());
    }
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_loopback_endpoints_with_explicit_port() {
        assert!(validate_cdp_url("http://127.0.0.1:9222").is_ok());
        assert!(validate_cdp_url("http://localhost:9222/").is_ok());
        assert!(validate_cdp_url("ws://[::1]:9222/devtools/browser/abc").is_ok());
    }

    #[test]
    fn refuses_remote_hosts_missing_ports_credentials_and_bad_schemes() {
        assert!(validate_cdp_url("http://10.0.0.5:9222").is_err());
        assert!(validate_cdp_url("http://example.com:9222").is_err());
        assert!(validate_cdp_url("http://127.0.0.1").is_err());
        assert!(validate_cdp_url("http://user:pw@127.0.0.1:9222").is_err());
        assert!(validate_cdp_url("https://127.0.0.1:9222").is_err());
        assert!(validate_cdp_url("file:///tmp/x").is_err());
        assert!(validate_cdp_url("not a url").is_err());
    }
}

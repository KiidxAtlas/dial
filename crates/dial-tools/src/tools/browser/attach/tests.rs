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

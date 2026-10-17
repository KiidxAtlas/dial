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

use super::*;

fn blocked(text: &str) -> bool {
    blocked_reason(text.parse().unwrap()).is_some()
}

#[test]
fn refuses_local_and_private_ranges() {
    for ip in [
        "127.0.0.1",
        "127.9.9.9",
        "0.0.0.0",
        "10.1.2.3",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "100.127.255.255",
        "198.18.0.1",
        "224.0.0.1",
        "255.255.255.255",
        "240.0.0.1",
        "::1",
        "::",
        "fc00::1",
        "fd12:3456::1",
        "fe80::1",
        "ff02::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "64:ff9b::a00:1",
        "2002:7f00:1::1",
        "::127.0.0.1",
    ] {
        assert!(blocked(ip), "{ip} should be refused");
    }
}

#[test]
fn allows_public_addresses() {
    for ip in [
        "93.184.216.34",
        "1.1.1.1",
        "8.8.8.8",
        "172.32.0.1",
        "100.63.255.255",
        "2606:2800:220:1:248:1893:25c8:1946",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
    ] {
        assert!(!blocked(ip), "{ip} should be allowed");
    }
}

#[test]
fn names_the_reason() {
    assert_eq!(
        blocked_reason("169.254.169.254".parse().unwrap()),
        Some("link-local (cloud metadata)")
    );
    assert_eq!(blocked_reason("10.0.0.1".parse().unwrap()), Some("private"));
}

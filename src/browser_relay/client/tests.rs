use std::net::{Ipv4Addr, SocketAddr, TcpListener};

use super::*;

#[test]
fn an_empty_relay_url_means_the_default_loopback_port() {
    assert_eq!(parse_relay_url("").unwrap(), RelayAddress { port: 9224 });
    assert_eq!(parse_relay_url("  ").unwrap(), RelayAddress { port: 9224 });
}

#[test]
fn localhost_and_loopback_with_a_port_are_accepted() {
    assert_eq!(
        parse_relay_url("http://localhost:9300").unwrap(),
        RelayAddress { port: 9300 }
    );
    assert_eq!(
        parse_relay_url("http://127.0.0.1:9301/").unwrap(),
        RelayAddress { port: 9301 }
    );
}

#[test]
fn remote_hosts_https_credentials_and_missing_ports_are_refused() {
    assert!(
        parse_relay_url("http://example.com:9224")
            .unwrap_err()
            .contains("127.0.0.1")
    );
    assert!(
        parse_relay_url("https://127.0.0.1:9224")
            .unwrap_err()
            .contains("http://")
    );
    assert!(
        parse_relay_url("http://user:pw@127.0.0.1:9224")
            .unwrap_err()
            .contains("credentials")
    );
    assert!(
        parse_relay_url("http://127.0.0.1")
            .unwrap_err()
            .contains("explicit port")
    );
    assert!(
        parse_relay_url("http://127.0.0.1:9224/json")
            .unwrap_err()
            .contains("relay root")
    );
}

#[test]
fn the_waiting_reason_carries_the_relay_error_and_install_hint() {
    let reason = waiting_reason(r#"{"error":"not connected","lastError":"tab 12 busy"}"#);
    assert!(reason.contains("not connected"), "{reason}");
    assert!(reason.contains("tab 12 busy"), "{reason}");
    assert!(waiting_reason("not json").contains("dial browser-relay install"));
}

#[test]
fn a_cancelled_wait_returns_at_once_and_still_starts_the_relay() {
    let dir = tempfile::tempdir().unwrap();
    let port = {
        let probe = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        probe.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}");
    let started = Instant::now();
    let error = resolve(&url, dir.path(), &|| true).unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    assert!(started.elapsed() < WAIT_LIMIT);
    // The relay is now listening: the port accepts connections.
    assert!(std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
}

#[test]
fn a_port_held_by_another_program_is_reported_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let holder = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let port = holder.local_addr().unwrap().port();
    // The holder answers nothing HTTP-wise, so the probe fails and the bind is attempted.
    let error = resolve(&format!("http://127.0.0.1:{port}"), dir.path(), &|| true).unwrap_err();
    assert!(error.contains("used by another program"), "{error}");
}

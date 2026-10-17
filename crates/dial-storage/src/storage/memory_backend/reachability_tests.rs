use super::{Reachability, probe_endpoint};
use std::{net::TcpListener, time::Duration};
#[test]
fn probe_accepts_a_listening_endpoint_and_refuses_a_closed_port() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let open = format!("http://{}/", listener.local_addr().unwrap());
    assert_eq!(probe_endpoint(&open, Duration::from_secs(2)), Ok(()));
    let closed_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let closed = format!("http://127.0.0.1:{closed_port}/");
    assert!(probe_endpoint(&closed, Duration::from_secs(2)).is_err());
}
#[test]
fn unreachable_probe_is_described_for_status() {
    assert_eq!(
        Reachability::Unreachable("no answer".into()).describe(),
        "unreachable: no answer"
    );
}

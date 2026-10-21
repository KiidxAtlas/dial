use super::*;
use std::net::TcpListener;

fn next(after: Duration) -> NextTimeout {
    NextTimeout {
        after: ureq::unversioned::transport::time::Duration::Exact(after),
        reason: ureq::Timeout::RecvResponse,
    }
}

#[test]
fn connections_probe_a_silent_peer_and_skip_unusable_addresses() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    // A port released right away refuses connections, so the next address is tried.
    let refused = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let stream = connect_any(
        &[refused, listener.local_addr().unwrap()],
        next(Duration::from_secs(5)),
    )
    .unwrap();
    keepalive(&stream).unwrap();
    let socket = SockRef::from(&stream);
    assert!(socket.keepalive().unwrap());
    assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
    assert_eq!(socket.tcp_keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
    assert_eq!(socket.tcp_keepalive_retries().unwrap(), KEEPALIVE_PROBES);
}

#[test]
fn an_expired_read_is_a_phase_timeout_but_a_dropped_connection_is_not() {
    let timeout = next(Duration::from_millis(100));
    let expired = io_failure(io::ErrorKind::WouldBlock.into(), timeout);
    assert!(matches!(
        expired,
        ureq::Error::Timeout(ureq::Timeout::RecvResponse)
    ));
    let dropped = io_failure(io::ErrorKind::TimedOut.into(), timeout);
    if cfg!(windows) {
        assert!(matches!(dropped, ureq::Error::Timeout(_)));
    } else {
        assert!(
            matches!(&dropped, ureq::Error::Io(error) if error.kind() == io::ErrorKind::TimedOut)
        );
    }
}

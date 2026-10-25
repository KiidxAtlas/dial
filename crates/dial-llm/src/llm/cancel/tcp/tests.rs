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
    // socket2 exposes the keepalive timing setters on Windows, but not these getters.
    #[cfg(not(windows))]
    {
        assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
        assert_eq!(socket.tcp_keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
    }
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

fn transport(stream: TcpStream, interrupt: &Arc<AtomicBool>, size: usize) -> KeepaliveTransport {
    let mut transport = KeepaliveTransport {
        stream,
        buffers: LazyBuffers::new(1024, size),
        interrupt: Arc::clone(interrupt),
        read_timeout: None,
        write_timeout: None,
    };
    for (index, byte) in transport.buffers.output().iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    transport
}

fn pattern_prefix(received: &[u8]) -> bool {
    received
        .iter()
        .enumerate()
        .all(|(index, byte)| *byte == (index % 251) as u8)
}

#[test]
fn a_write_to_a_reading_peer_sends_every_byte_exactly_once() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let reader = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        peer.read_to_end(&mut received).unwrap();
        received
    });
    let interrupt = Arc::new(AtomicBool::new(false));
    let size = 4 << 20;
    let mut sender = transport(TcpStream::connect(addr).unwrap(), &interrupt, size);
    sender
        .transmit_output(size, next(Duration::from_secs(30)))
        .unwrap();
    drop(sender);
    let received = reader.join().unwrap();
    assert_eq!(received.len(), size);
    assert!(pattern_prefix(&received));
}

#[test]
fn an_interrupt_stops_a_blocked_write_without_replaying_sent_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    // Bound both buffers rather than relying on platform defaults (Windows can buffer the
    // entire request). Set the receive window before the handshake so it starts out small.
    SockRef::from(&listener).set_recv_buffer_size(4096).unwrap();
    let addr = listener.local_addr().unwrap();
    let stream = TcpStream::connect(addr).unwrap();
    SockRef::from(&stream).set_send_buffer_size(4096).unwrap();
    // The peer never reads during the write, so the request blocks on the bounded buffers.
    let (mut peer, _) = listener.accept().unwrap();
    let interrupt = Arc::new(AtomicBool::new(false));
    let size = 64 << 20;
    let mut sender = transport(stream, &interrupt, size);
    let flag = Arc::clone(&interrupt);
    let interrupter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = Instant::now();
    let error = sender
        .transmit_output(size, next(Duration::from_secs(60)))
        .unwrap_err();
    interrupter.join().unwrap();
    assert!(matches!(error, ureq::Error::Other(_)), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the interrupt was noticed, not the 60 s write budget"
    );
    drop(sender);
    let mut received = Vec::new();
    peer.read_to_end(&mut received).unwrap();
    assert!(
        !received.is_empty(),
        "the write made progress before cancellation"
    );
    assert!(received.len() < size);
    assert!(pattern_prefix(&received), "sent bytes are an exact prefix");
}

#[test]
fn an_interrupted_request_does_not_start_a_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let interrupt = AtomicBool::new(true);
    let error = connect_cancellable(
        &[listener.local_addr().unwrap()],
        next(Duration::from_secs(5)),
        &interrupt,
    )
    .unwrap_err();
    assert!(matches!(error, ureq::Error::Other(_)), "{error:?}");
    let interrupt = AtomicBool::new(false);
    assert!(
        connect_cancellable(
            &[listener.local_addr().unwrap()],
            next(Duration::from_secs(5)),
            &interrupt,
        )
        .is_ok()
    );
}

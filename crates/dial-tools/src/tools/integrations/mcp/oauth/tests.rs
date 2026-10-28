use super::*;
use std::io::{BufRead, BufReader, Read};
use std::sync::{atomic::Ordering, mpsc};
use std::time::Instant;

#[test]
fn slow_refresh_obeys_request_cancellation_and_releases_its_socket() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let (entered, rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(&mut socket);
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        entered.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(reader.read(&mut byte).unwrap(), 0);
    });
    let source = Arc::new(TokenSource::empty(PathBuf::new()));
    *lock(&source.grant) = Some(Grant {
        access: "expired".into(),
        refresh: "refresh".into(),
        token_endpoint: endpoint,
        ..Grant::default()
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_source = source.clone();
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || worker_source.bearer_with(&worker_cancel));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let began = Instant::now();
    cancel.store(true, Ordering::SeqCst);
    assert!(worker.join().unwrap().is_none());
    assert!(began.elapsed() < Duration::from_secs(1));
    assert_eq!(source.stored_access().as_deref(), Some("expired"));
    server.join().unwrap();
}

#[test]
fn waiting_for_another_refresh_is_cancellable_without_spending_the_token() {
    let source = Arc::new(TokenSource::empty(PathBuf::new()));
    let turn = source.refreshing.lock();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_source = source.clone();
    let worker_cancel = cancel.clone();
    let worker = std::thread::spawn(move || worker_source.refresh_with("stale", &worker_cancel));
    cancel.store(true, Ordering::SeqCst);
    assert!(!worker.join().unwrap());
    drop(turn);
}

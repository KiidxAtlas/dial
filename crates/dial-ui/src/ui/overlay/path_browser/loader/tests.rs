use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn stalled_work_does_not_block_input_and_latest_request_wins() {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let loader = Loader::start(move |request, _| {
        let Request::Validate(path) = request else {
            panic!("unexpected request")
        };
        if path == std::path::Path::new("first") {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        Reply::Validated(path)
    })
    .unwrap();
    loader.request(Request::Validate("first".into()));
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // These calls return while the first filesystem operation remains deliberately blocked.
    loader.request(Request::Validate("superseded".into()));
    loader.request(Request::Validate("latest".into()));
    assert!(loader.collect().is_none());
    release_tx.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(reply) = loader.collect() {
            assert!(
                matches!(reply, Reply::Validated(path) if path == std::path::Path::new("latest"))
            );
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
}

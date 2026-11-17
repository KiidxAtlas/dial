use super::*;
use std::thread;

#[test]
fn admission_bounds_bytes_and_count_and_releases_capacity_on_receive() {
    let (tx, rx) = channel(2, 10, String::len);
    tx.try_send("123456".to_owned()).unwrap();
    assert!(matches!(
        tx.try_send("12345".to_owned()),
        Err(TrySendError::Full(_))
    ));
    tx.try_send("1234".to_owned()).unwrap();
    assert!(matches!(
        tx.try_send(String::new()),
        Err(TrySendError::Full(_))
    ));
    assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), "123456");
    tx.try_send("abcdef".to_owned()).unwrap();
    assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), "1234");
    assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), "abcdef");
    // Preserve the existing frame limit: a legal frame larger than the queue budget still fits,
    // but no second frame can be buffered beside it.
    tx.try_send("x".repeat(20)).unwrap();
    assert!(matches!(
        tx.try_send("y".to_owned()),
        Err(TrySendError::Full(_))
    ));
    assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap().len(), 20);
}

#[test]
fn blocking_producer_is_released_by_drain_or_receiver_drop() {
    for disconnect in [false, true] {
        let (tx, rx) = channel(10, 1, String::len);
        tx.send("a".to_owned()).unwrap();
        let worker = thread::spawn(move || tx.send("b".to_owned()));
        if disconnect {
            drop(rx);
            assert!(worker.join().unwrap().is_err());
        } else {
            assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), "a");
            worker.join().unwrap().unwrap();
            assert_eq!(rx.recv_timeout(Duration::ZERO).unwrap(), "b");
            assert!(matches!(
                rx.recv_timeout(Duration::ZERO),
                Err(RecvTimeoutError::Disconnected)
            ));
        }
    }
}

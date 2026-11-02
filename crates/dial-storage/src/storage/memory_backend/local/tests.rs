use super::*;

#[test]
fn renewal_distinguishes_busy_from_expired_or_reassigned_lease() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("lease.sqlite3");
    let mut owner = Connection::open(&path).unwrap();
    owner.execute_batch("CREATE TABLE consolidation(id INTEGER PRIMARY KEY,lease TEXT,lease_until INTEGER); INSERT INTO consolidation VALUES(1,'owner',100);").unwrap();
    let heartbeat = Connection::open(&path).unwrap();
    heartbeat.busy_timeout(std::time::Duration::ZERO).unwrap();
    let lock = owner
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(renew_lease(&heartbeat, "owner", 50, 100).unwrap());
    lock.rollback().unwrap();
    assert!(renew_lease(&heartbeat, "owner", 50, 100).unwrap());
    assert!(!renew_lease(&heartbeat, "other", 60, 100).unwrap());
    assert!(!renew_lease(&heartbeat, "owner", 151, 100).unwrap());
}

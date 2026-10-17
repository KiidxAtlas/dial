use super::*;

fn record(id: &str, proc: ProcId) -> HostRecord {
    HostRecord {
        id: id.into(),
        pid: proc.pid as u32,
        proc,
        session_id: "s1".into(),
        cwd: "/tmp".into(),
        model: "m".into(),
        read_only: true,
        guests: 2,
        started_ms: 5,
    }
}

#[test]
fn lists_live_hosts_and_prunes_dead_and_recycled_ones() {
    let dir = tempfile::tempdir().unwrap();
    let live_id = ProcId::current().unwrap();
    let dead_id = ProcId {
        pid: i32::MAX - 1,
        start: 1,
    };
    let recycled_id = ProcId {
        start: live_id.start.wrapping_add(1),
        ..live_id
    };
    let live = write(dir.path(), &record("live-1", live_id)).unwrap();
    let dead = write(dir.path(), &record("dead-1", dead_id)).unwrap();
    let recycled = write(dir.path(), &record("recycled-1", recycled_id)).unwrap();
    let hosts = list_live(dir.path()).unwrap();
    assert_eq!(hosts, vec![record("live-1", live_id)]);
    assert!(live.exists());
    assert!(!dead.exists(), "record of a dead process is pruned");
    assert!(!recycled.exists(), "record whose PID was reused is pruned");
    remove(&live);
    assert!(list_live(dir.path()).unwrap().is_empty());
}

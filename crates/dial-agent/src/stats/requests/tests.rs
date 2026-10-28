use crate::stats::{Db, RequestRecord};

fn record(ts_ms: i64) -> RequestRecord {
    RequestRecord {
        ts_ms,
        session: "s1".into(),
        project: "proj-0000aaaa".into(),
        model: "m".into(),
        input: 1,
        output: 1,
        reported: true,
        duration_ms: 5,
        finish_reason: "stop".into(),
        ..Default::default()
    }
}

#[test]
fn recent_requests_are_newest_first_and_limited() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(dir.path()).unwrap();
    for ts in [1_000, 2_000, 3_000] {
        db.record_request(&record(ts)).unwrap();
    }
    let rows = db.recent_requests(None, 2).unwrap();
    assert_eq!(
        rows.iter().map(|r| r.ts_ms).collect::<Vec<_>>(),
        vec![3_000, 2_000]
    );
}

#[test]
fn recent_requests_respect_the_lower_time_bound() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(dir.path()).unwrap();
    for ts in [1_000, 2_000, 3_000] {
        db.record_request(&record(ts)).unwrap();
    }
    let rows = db.recent_requests(Some(2_000), 10).unwrap();
    assert_eq!(
        rows.iter().map(|r| r.ts_ms).collect::<Vec<_>>(),
        vec![3_000, 2_000]
    );
}

#[test]
fn request_detail_is_found_by_row_id_and_absent_otherwise() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open_default(dir.path()).unwrap();
    db.record_request(&record(1_000)).unwrap();
    let id = db.recent_requests(None, 1).unwrap()[0].id;
    let row = db.request(id).unwrap().expect("recorded request is found");
    assert_eq!(row.ts_ms, 1_000);
    assert_eq!(row.model, "m");
    assert!(db.request(id + 1_000).unwrap().is_none());
}

#[test]
fn live_session_is_none_without_any_session() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_default(dir.path()).unwrap();
    assert!(db.live(dir.path(), 1_000).unwrap().is_none());
}

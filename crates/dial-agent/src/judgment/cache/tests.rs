use super::*;

fn open(dir: &Path, ttl: u64, rows: u64) -> Cache {
    Cache::open(&dir.join("judgment.db"), ttl, rows).unwrap()
}

#[test]
fn miss_then_hit_returns_stored_choice() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open(dir.path(), 100, 10);
    assert_eq!(cache.get("k", 1000).unwrap(), None);
    cache.put("k", "high", 1000).unwrap();
    assert_eq!(cache.get("k", 1001).unwrap().as_deref(), Some("high"));
}

#[test]
fn entries_expire_at_the_ttl_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open(dir.path(), 100, 10);
    cache.put("k", "low", 1000).unwrap();
    // Written at 1000 with ttl 100: still valid at 1100, gone one second later.
    assert!(cache.get("k", 1100).unwrap().is_some());
    assert!(cache.get("k", 1101).unwrap().is_none());
}

#[test]
fn reads_refresh_recency_so_the_least_recent_row_is_evicted() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open(dir.path(), 10_000, 2);
    cache.put("a", "1", 1).unwrap();
    cache.put("b", "2", 2).unwrap();
    // Touch "a" so "b" becomes the least recently used row.
    assert!(cache.get("a", 3).unwrap().is_some());
    cache.put("c", "3", 4).unwrap();
    assert_eq!(cache.len().unwrap(), 2);
    assert!(cache.get("a", 5).unwrap().is_some());
    assert!(cache.get("b", 5).unwrap().is_none());
    assert!(cache.get("c", 5).unwrap().is_some());
}

#[test]
fn rewriting_a_key_replaces_its_choice() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open(dir.path(), 100, 10);
    cache.put("k", "low", 1).unwrap();
    cache.put("k", "high", 2).unwrap();
    assert_eq!(cache.get("k", 3).unwrap().as_deref(), Some("high"));
    assert_eq!(cache.len().unwrap(), 1);
}

#[test]
fn override_path_wins_and_empty_user_dir_disables_the_default() {
    let user = Path::new("/state/dial");
    assert_eq!(
        resolve_path(Some("/tmp/x.db".into()), user),
        Some(PathBuf::from("/tmp/x.db"))
    );
    assert_eq!(
        resolve_path(None, user),
        Some(user.join("cache/judgment.db"))
    );
    assert_eq!(
        resolve_path(Some(OsString::new()), user),
        Some(user.join("cache/judgment.db"))
    );
    assert_eq!(resolve_path(None, Path::new("")), None);
}

#[test]
fn a_future_schema_version_is_refused_rather_than_misread() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("judgment.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    let error = Cache::open(&path, 100, 10)
        .err()
        .expect("refused")
        .to_string();
    assert!(error.contains("schema 99"), "{error}");
}

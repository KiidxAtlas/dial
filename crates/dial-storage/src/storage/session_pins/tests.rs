use super::*;

#[test]
fn toggle_round_trips_and_sorts_pinned_first() {
    let dir = tempfile::tempdir().unwrap();
    assert!(toggle(dir.path(), "b").unwrap());
    assert!(toggle(dir.path(), "a").unwrap());
    assert_eq!(load(dir.path()).unwrap(), vec!["b", "a"]);
    assert!(!toggle(dir.path(), "b").unwrap());
    assert_eq!(load(dir.path()).unwrap(), vec!["a"]);
    unpin(dir.path(), "a").unwrap();
    assert!(load(dir.path()).unwrap().is_empty());

    let pinned = vec!["c".to_string()];
    let sorted = sort_pinned_first(vec!["a", "c", "b"], &pinned, |s| s);
    assert_eq!(sorted, vec!["c", "a", "b"]);
}

use super::*;
#[test]
fn unrelated_documents_do_not_wait_for_another_writer() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    let held = Lease::acquire([a.clone()]).unwrap();
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let _other = Lease::acquire([b]).unwrap();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(1))
            .expect("unrelated document must be independently available");
    });
    drop(held);
    let higher = Lease::acquire([a.clone(), dir.path().join("z")]).unwrap();
    assert!(
        Lease::acquire([dir.path().join("b")]).is_err(),
        "nested batches must preserve acquisition order"
    );
    drop(higher);
}
#[test]
fn disjoint_edits_merge_and_conflicts_preserve_original() {
    assert_eq!(
        merge(
            "left=0\nright=0\n",
            "left=1\nright=0\n",
            "left=0\nright=2\n"
        )
        .as_deref(),
        Some("left=1\nright=2\n")
    );
    assert!(merge("x=0\n", "x=1\n", "x=2\n").is_none());
    assert!(merge("a\nb\n", "a\nx\nb\n", "a\ny\nb\n").is_none());
    assert_eq!(
        merge("α\nβ\nγ", "Α\nβ\nγ", "α\nβ\nΓ").as_deref(),
        Some("Α\nβ\nΓ")
    );
}
#[test]
fn independent_workspaces_commit_stale_nonoverlapping_proposals() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("race.txt"), "left=0\nright=0\n").unwrap();
    let a = Workspace::new(dir.path()).unwrap();
    let b = Workspace::new(dir.path()).unwrap();
    let one = Change::load(
        &a,
        a.root().join("race.txt"),
        Some("left=1\nright=0\n".into()),
    )
    .unwrap();
    let two = Change::load(
        &b,
        b.root().join("race.txt"),
        Some("left=0\nright=2\n".into()),
    )
    .unwrap();
    std::thread::scope(|scope| {
        let x = scope.spawn(|| super::super::editing::finish(&a, vec![one], true));
        let y = scope.spawn(|| super::super::editing::finish(&b, vec![two], true));
        x.join().unwrap().unwrap();
        y.join().unwrap().unwrap();
    });
    assert_eq!(
        std::fs::read_to_string(dir.path().join("race.txt")).unwrap(),
        "left=1\nright=2\n"
    );
}

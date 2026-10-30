use super::*;
#[test]
fn acknowledged_live_baselines_survive_replacement_and_old_handle_appends() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session.jsonl");
    let journal = Journal::open(&session).unwrap();
    let alias = Journal::open(&session).unwrap();
    let tree = crate::runtime::Tree::default();
    tree.journal(journal.clone());
    let body = tree.start("assistant", "live", None);
    body.append("first");
    let node = body.snapshot();
    let acknowledged = BTreeMap::from([(node.id.clone(), node.result.revision)]);
    alias.checkpoint(&acknowledged).unwrap();
    body.append(" second");
    journal.check().unwrap();
    let recovered = recover(&session).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].result.text, "first second");
    assert_eq!(recovered[0].result.revision, body.revision());
}
#[test]
fn only_durably_acknowledged_settled_nodes_are_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = crate::runtime::Tree::default();
    tree.journal(journal.clone());
    let settled = tree.start("tool", "settled", None);
    settled.settle("done".into(), None, crate::runtime::Phase::Completed);
    let abandoned = tree.start("tool", "abandoned branch", None);
    abandoned.settle("preserve".into(), None, crate::runtime::Phase::Interrupted);
    journal.checkpoint(&BTreeMap::new()).unwrap();
    assert_eq!(recover(&session).unwrap().len(), 2);
    let node = settled.snapshot();
    journal
        .checkpoint(&BTreeMap::from([(node.id, node.result.revision)]))
        .unwrap();
    let recovered = recover(&session).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].result.text, "preserve");
}

#[test]
fn relocation_updates_all_publishers_and_owner_removal_refuses_stale_writes() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.jsonl");
    let destination = root.path().join("archive.jsonl");
    let journal = Journal::open(&source).unwrap();
    let tree = crate::runtime::Tree::default();
    tree.journal(journal.clone());
    let body = tree.start("assistant", "live", None);
    body.append("before");
    relocate(&source, &destination).unwrap();
    body.append(" after");
    journal.checkpoint(&BTreeMap::new()).unwrap();
    assert!(!path(&source).exists());
    assert_eq!(
        recover(&destination).unwrap()[0].result.text,
        "before after"
    );
    remove(&destination).unwrap();
    body.append(" stale");
    assert!(journal.check().is_err());
    assert!(!path(&destination).exists());
}

#[test]
fn relocation_never_replaces_an_existing_destination() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.jsonl");
    let destination = root.path().join("archive.jsonl");
    std::fs::write(&source, "source").unwrap();
    // Created after `relocate`'s early check would have run: the publish step itself refuses.
    std::fs::write(&destination, "other").unwrap();
    let error = publish_no_replace(&source, &destination).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "source");
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "other");
    std::fs::remove_file(&destination).unwrap();
    publish_no_replace(&source, &destination).unwrap();
    assert!(!source.exists());
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "source");
}

#[test]
fn future_stream_versions_are_preserved_and_refused_before_append() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("future.jsonl");
    let bytes = b"{\"schema_version\":3,\"kind\":\"future\"}\n";
    std::fs::write(path(&session), bytes).unwrap();
    assert!(recover(&session).is_err());
    assert!(Journal::open(&session).is_err());
    assert_eq!(std::fs::read(path(&session)).unwrap(), bytes);
}

#[test]
fn copy_creates_only_a_fresh_destination_and_keeps_an_existing_one() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.jsonl");
    let destination = root.path().join("copy.jsonl");
    // Empty sources still reserve the destination and refuse an existing journal.
    copy(&source, &destination).unwrap();
    assert_eq!(std::fs::read(path(&destination)).unwrap(), b"");
    assert!(copy(&source, &destination).is_err());
    remove(&destination).unwrap();
    let journal = Journal::open(&source).unwrap();
    let tree = crate::runtime::Tree::default();
    tree.journal(journal.clone());
    let body = tree.start("assistant", "live", None);
    body.append("kept");
    copy(&source, &destination).unwrap();
    let before = std::fs::read(path(&destination)).unwrap();
    assert!(copy(&source, &destination).is_err());
    assert_eq!(std::fs::read(path(&destination)).unwrap(), before);
}

#[test]
fn aggregate_live_baselines_above_the_private_limit_defer_without_poisoning_the_writer() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = crate::runtime::Tree::default();
    tree.journal(journal.clone());
    // Each body is valid alone; together their retained baselines exceed the private limit.
    let chunk = "x".repeat(3 * 1024 * 1024);
    let bodies: Vec<_> = (0..3)
        .map(|index| {
            let body = tree.start("assistant", &format!("live {index}"), None);
            body.append(&chunk);
            body
        })
        .collect();
    let ids: Vec<String> = bodies.iter().map(|body| body.snapshot().id).collect();
    journal.checkpoint(&BTreeMap::new()).unwrap();
    journal.check().unwrap();
    assert!(std::fs::metadata(path(&session)).unwrap().len() > RECLAIM_AT);
    // Unrelated writes keep flowing after the deferred reclamation.
    bodies[0].append(" tail");
    journal.check().unwrap();
    let revisions: Vec<u64> = bodies
        .iter()
        .map(|body| body.snapshot().result.revision)
        .collect();
    drop(bodies);
    drop(tree);
    drop(journal);
    let reopened = Journal::open(&session).unwrap();
    reopened.check().unwrap();
    let recovered = recover(&session).unwrap();
    assert_eq!(recovered.len(), 3);
    for (index, (id, revision)) in ids.iter().zip(&revisions).enumerate() {
        let node = recovered.iter().find(|node| &node.id == id).unwrap();
        assert_eq!(node.result.revision, *revision);
        if index == 0 {
            assert!(node.result.text.ends_with(" tail"));
        }
    }
}

#[test]
fn oversized_row_is_skipped_and_never_compacted_away() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = crate::runtime::Tree::default();
    tree.journal(journal.clone());
    let body = tree.start("assistant", "after", None);
    body.append("kept");
    drop(body);
    drop(tree);
    drop(journal);
    let stream = path(&session);
    let original = std::fs::read(&stream).unwrap();
    // An unparseable row larger than the replay limit, followed by valid frames.
    let mut bytes = vec![b'x'; MAX_FRAME];
    bytes.push(b'\n');
    bytes.extend_from_slice(&original);
    std::fs::write(&stream, &bytes).unwrap();
    let (nodes, complete) = scan(&session).unwrap();
    assert!(!complete);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].result.text, "kept");
    assert!(copy(&session, &root.path().join("copy.jsonl")).is_err());
    let journal = Journal::open(&session).unwrap();
    journal.checkpoint(&BTreeMap::new()).unwrap();
    journal.check().unwrap();
    assert!(std::fs::metadata(&stream).unwrap().len() >= MAX_FRAME as u64);
    assert_eq!(recover(&session).unwrap()[0].result.text, "kept");
}

#[test]
fn torn_final_row_recovers_the_complete_prefix() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session.jsonl");
    let journal = Journal::open(&session).unwrap();
    let tree = crate::runtime::Tree::default();
    tree.journal(journal.clone());
    let body = tree.start("assistant", "prefix", None);
    body.append("kept");
    drop(body);
    drop(tree);
    drop(journal);
    let stream = path(&session);
    let mut bytes = std::fs::read(&stream).unwrap();
    bytes.extend_from_slice(b"{\"kind\":\"append\",\"id\":\"torn");
    std::fs::write(&stream, &bytes).unwrap();
    let (nodes, complete) = scan(&session).unwrap();
    assert!(complete);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].result.text, "kept");
}

#[test]
fn retry_never_adopts_replaced_truncated_or_owner_removed_journals() {
    for case in ["replaced", "truncated", "removed"] {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session.jsonl");
        let stream = path(&session);
        let journal = Journal::open(&session).unwrap();
        let tree = crate::runtime::Tree::default();
        tree.journal(journal.clone());
        let body = tree.start("assistant", "live", None);
        body.append("prefix");
        journal.fail_writes_for_test(File::open(&stream).unwrap());
        body.append("failed");
        assert!(journal.check().is_err());
        match case {
            "replaced" => {
                std::fs::rename(&stream, root.path().join("original.streams")).unwrap();
                std::fs::write(&stream, b"unrelated").unwrap();
            }
            "truncated" => std::fs::write(&stream, b"short").unwrap(),
            "removed" => remove(&session).unwrap(),
            _ => unreachable!(),
        }
        let before = std::fs::read(&stream).ok();
        assert!(journal.retry_failed_write().is_err(), "{case}");
        assert!(journal.check().is_err(), "{case}");
        assert_eq!(std::fs::read(&stream).ok(), before, "{case}");
    }
}

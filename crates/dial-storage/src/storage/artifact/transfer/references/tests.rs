use super::*;
use serde_json::json;

fn evidence(store: &ArtifactStore, bytes: &[u8], parents: Vec<String>) -> String {
    let evidence = serde_json::from_value(json!({
        "source":"file:///never-fetched", "location":{"kind":"whole"},
        "method":"read", "producer":"reference-test", "kind":"observation",
        "summary":"retained", "representation":{"media_type":"text/plain",
        "sha256":format!("{:x}", Sha256::digest(bytes))}, "limitations":[],
        "context":{"parents":parents}
    }))
    .unwrap();
    store.retain_evidence(&evidence, bytes).unwrap()
}

#[test]
fn authenticated_manifest_chain_expands_without_scanning_ordinary_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let unrelated = store.retain("unrelated").unwrap();
    let parent = evidence(&store, b"raw parent", vec![]);
    let child = evidence(&store, unrelated.as_bytes(), vec![parent.clone()]);
    let mut refs = HashSet::from([child.clone()]);
    store.expand_evidence_references(&mut refs).unwrap();
    assert_eq!(refs.len(), 4);
    assert!(refs.contains(&parent));
    assert!(refs.contains(&store.evidence_record(&parent).unwrap().representation_uri));
    assert!(refs.contains(&store.evidence_record(&child).unwrap().representation_uri));
    assert!(!refs.contains(&unrelated));
}

#[test]
fn missing_representation_stays_a_dependency_and_corrupt_manifest_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let citation = evidence(&store, b"missing", vec![]);
    let representation = store.evidence_record(&citation).unwrap().representation_uri;
    fs::remove_file(store.blob_path(&representation).unwrap()).unwrap();
    let mut refs = HashSet::from([citation.clone()]);
    store.expand_evidence_references(&mut refs).unwrap();
    assert!(refs.contains(&representation));
    let mut record = store.evidence_record(&citation).unwrap();
    record.captured_at_unix_ms += 1;
    fs::write(
        store.blob_path(&citation).unwrap(),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    assert!(
        store
            .expand_evidence_references(&mut HashSet::from([citation]))
            .is_err()
    );
}

#[test]
fn member_limit_refuses_partial_reference_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let citation = evidence(&store, b"representation", vec![]);
    let mut record = store.evidence_record(&citation).unwrap();
    let mut refs = HashSet::new();
    for group in 0..64 {
        record.evidence.context.parents = (0..64)
            .map(|n| format!("artifact://{:064x}", group * 64 + n))
            .collect();
        refs.insert(
            store
                .retain_exact_bytes(&serde_json::to_vec(&record).unwrap())
                .unwrap(),
        );
    }
    assert!(
        store
            .expand_evidence_references(&mut refs)
            .unwrap_err()
            .to_string()
            .contains("member limit")
    );
}

#[test]
fn snapshots_and_clock_basis_are_dependencies_but_source_paths_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let citation = evidence(&store, b"representation", vec![]);
    let mut record = store.evidence_record(&citation).unwrap();
    let snapshot = store.retain("original").unwrap();
    let basis = evidence(&store, b"clock basis", vec![]);
    record.evidence.context = serde_json::from_value(json!({
        "snapshot":{"sha256":snapshot.strip_prefix("artifact://").unwrap(), "bytes":8,
            "captured_at_unix_ms":1, "artifact":snapshot},
        "alignments":[{"from":"media", "to":"logs", "offset_ms":1,
            "uncertainty_ms":2, "basis":[basis]}]
    }))
    .unwrap();
    let reference = store
        .retain_exact_bytes(&serde_json::to_vec(&record).unwrap())
        .unwrap();
    let mut refs = HashSet::from([reference]);
    store.expand_evidence_references(&mut refs).unwrap();
    assert!(refs.contains(&snapshot));
    assert!(refs.contains(&basis));
    assert_eq!(refs.len(), 5);
}

#[test]
fn depth_limit_refuses_partial_graph() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let mut parents = vec![];
    for _ in 0..=MAX_DEPTH + 1 {
        parents = vec![evidence(&store, b"same representation", parents)];
    }
    assert!(
        store
            .expand_evidence_references(&mut parents.into_iter().collect())
            .unwrap_err()
            .to_string()
            .contains("depth limit")
    );
}

#[test]
fn byte_limit_bounds_recognized_manifest_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let citation = evidence(&store, b"representation", vec![]);
    let mut record = store.evidence_record(&citation).unwrap();
    let basis: Vec<_> = (0..64).map(|n| format!("artifact://{n:064x}")).collect();
    let alignment = json!({"from":"media", "to":"logs", "offset_ms":1,
        "uncertainty_ms":2, "basis":basis});
    record.evidence.context =
        serde_json::from_value(json!({"alignments":vec![alignment;64]})).unwrap();
    record.captured_at_unix_ms = 0;
    let minimum_bytes = serde_json::to_vec(&record).unwrap().len() as u64;
    let mut refs = HashSet::new();
    for n in 0..=MAX_SCAN_BYTES / (2 * minimum_bytes) {
        record.captured_at_unix_ms = n;
        let bytes = serde_json::to_vec(&record).unwrap();
        refs.insert(store.retain_exact_bytes(&bytes).unwrap());
    }
    assert!(
        store
            .expand_evidence_references(&mut refs)
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
}

fn ordinary_blob(store: &ArtifactStore, bytes: &[u8]) -> String {
    let uri = format!("artifact://{:x}", Sha256::digest(bytes));
    fs::write(store.blob_path(&uri).unwrap(), bytes).unwrap();
    uri
}

#[test]
fn legacy_root_count_does_not_consume_evidence_member_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let mut refs: HashSet<_> = (0..=MAX_MEMBERS)
        .map(|n| ordinary_blob(&store, format!("ordinary output {n}").as_bytes()))
        .collect();
    let before = refs.clone();
    store.expand_evidence_references(&mut refs).unwrap();
    assert_eq!(refs, before);
}

#[test]
fn legacy_output_bytes_do_not_consume_evidence_read_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let mut refs = HashSet::new();
    let mut bytes = vec![b'x'; MAX_MANIFEST_BYTES as usize];
    for n in 0..=MAX_SCAN_BYTES / MAX_MANIFEST_BYTES {
        bytes[0] = n as u8;
        refs.insert(ordinary_blob(&store, &bytes));
    }
    let before = refs.clone();
    store.expand_evidence_references(&mut refs).unwrap();
    assert_eq!(refs, before);
}

#[test]
fn recognized_oversized_or_malformed_manifests_fail_instead_of_being_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let mut bytes = MANIFEST_PREFIX.to_vec();
    let malformed = ordinary_blob(&store, &bytes);
    assert!(
        store
            .expand_evidence_references(&mut HashSet::from([malformed]))
            .is_err()
    );
    bytes.resize(MAX_MANIFEST_BYTES as usize + 1, b' ');
    let oversized = ordinary_blob(&store, &bytes);
    assert!(
        store
            .expand_evidence_references(&mut HashSet::from([oversized]))
            .unwrap_err()
            .to_string()
            .contains("read limit")
    );
}

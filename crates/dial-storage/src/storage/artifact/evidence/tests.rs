use super::*;
use crate::storage::artifact::ArtifactConfig;
use serde_json::json;

fn evidence(bytes: &[u8]) -> Evidence {
    // The original envelope remains valid when optional context metadata is absent.
    serde_json::from_value(json!({
        "source":"file:///authorized/source", "location":{"kind":"whole"},
        "method":"read", "producer":"storage-test", "kind":"observation",
        "summary":"Retained bytes", "representation":{
            "media_type":"application/octet-stream", "sha256":format!("{:x}", Sha256::digest(bytes))
        }, "limitations":[]
    }))
    .unwrap()
}

fn store() -> (tempfile::TempDir, ArtifactStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn reopen_preserves_manifest_reference_and_exact_binary_representation() {
    let (dir, store) = store();
    let bytes = b"\0\xffretained\n";
    let citation = store.retain_evidence(&evidence(bytes), bytes).unwrap();
    let record = store.evidence_record(&citation).unwrap();
    assert_eq!(record.schema_version, 1);
    assert_eq!(record.evidence, evidence(bytes));
    assert!(record.captured_at_unix_ms > 0);
    let reopened = ArtifactStore::new(dir.path()).unwrap();
    assert_eq!(reopened.evidence_record(&citation).unwrap(), record);
    assert_eq!(
        reopened.evidence_bytes(&record, bytes.len()).unwrap(),
        bytes
    );
    assert!(reopened.evidence_bytes(&record, bytes.len() - 1).is_err());
    assert_eq!(reopened.info(&citation).unwrap()["state"], "retained");
    assert_eq!(
        reopened.info(&record.representation_uri).unwrap()["state"],
        "retained"
    );
}

#[test]
fn digest_mismatch_and_oversize_are_refused_without_truncation() {
    let (_dir, store) = store();
    assert!(
        store
            .retain_evidence(&evidence(b"expected"), b"different")
            .is_err()
    );
    let limited = store.with_limits(&ArtifactConfig {
        max_bytes: 8,
        head_bytes: 2,
    });
    assert!(
        limited
            .retain_exact_bytes(b"123456789")
            .unwrap_err()
            .to_string()
            .contains("refusing truncation")
    );
    let uri = limited.retain_exact_bytes(b"12345678").unwrap();
    assert_eq!(
        limited.read_verified_evidence_blob(&uri, 8).unwrap(),
        b"12345678"
    );
    assert!(
        limited.retain_evidence(&evidence(b"x"), b"x").is_err(),
        "manifest also obeys the configured spool cap"
    );
}

#[test]
fn missing_representation_reuses_retention_diagnostics() {
    let (_dir, store) = store();
    let citation = store.retain_evidence(&evidence(b"blob"), b"blob").unwrap();
    let record = store.evidence_record(&citation).unwrap();
    std::fs::remove_file(store.path(&record.representation_uri).unwrap()).unwrap();
    let error = store.evidence_bytes(&record, 100).unwrap_err().to_string();
    assert!(error.contains("unavailable in this project's retained store"));
    assert!(error.contains("Retention was recorded here"));
    // The immutable manifest remains readable even if its representation was reclaimed.
    assert_eq!(store.evidence_record(&citation).unwrap(), record);
}

#[test]
fn forged_manifest_hash_and_corrupt_existing_blobs_are_rejected() {
    let (_dir, store) = store();
    let citation = store.retain_evidence(&evidence(b"blob"), b"blob").unwrap();
    let record = store.evidence_record(&citation).unwrap();
    std::fs::write(store.path(&citation).unwrap(), b"{}").unwrap();
    assert!(
        store
            .evidence_record(&citation)
            .unwrap_err()
            .to_string()
            .contains("content hash mismatch")
    );
    std::fs::write(store.path(&record.representation_uri).unwrap(), b"evil").unwrap();
    assert!(store.evidence_bytes(&record, 100).is_err());
    assert!(
        store.retain_exact_bytes(b"blob").is_err(),
        "a matching filename alone is not integrity proof"
    );
}

#[test]
fn manifests_validate_version_and_representation_identity_and_size() {
    let (_dir, store) = store();
    let citation = store.retain_evidence(&evidence(b"blob"), b"blob").unwrap();
    let record = store.evidence_record(&citation).unwrap();
    let mut invalid = record.clone();
    invalid.schema_version = 2;
    let uri = store
        .retain_exact_bytes(&serde_json::to_vec(&invalid).unwrap())
        .unwrap();
    assert!(store.evidence_record(&uri).is_err());
    invalid = record;
    invalid.representation_uri = format!("artifact://{}", "a".repeat(64));
    let uri = store
        .retain_exact_bytes(&serde_json::to_vec(&invalid).unwrap())
        .unwrap();
    assert!(store.evidence_record(&uri).is_err());
    let uri = store
        .retain_exact_bytes(&vec![b' '; MAX_MANIFEST_BYTES + 1])
        .unwrap();
    assert!(
        store
            .evidence_record(&uri)
            .unwrap_err()
            .to_string()
            .contains("read limit")
    );
}

#[test]
fn lineage_requires_existing_valid_parent_manifest() {
    let (_dir, store) = store();
    let parent = store
        .retain_evidence(&evidence(b"parent"), b"parent")
        .unwrap();
    let mut child = evidence(b"child");
    child.context.parents = vec![parent];
    let citation = store.retain_evidence(&child, b"child").unwrap();
    assert_eq!(store.evidence_record(&citation).unwrap().evidence, child);
    child.context.parents = vec![format!("artifact://{}", "a".repeat(64))];
    assert!(
        store
            .retain_evidence(&child, b"child")
            .unwrap_err()
            .to_string()
            .contains("invalid evidence parent")
    );
    child.context.parents = vec![store.retain_exact_bytes(b"not a manifest").unwrap()];
    assert!(store.retain_evidence(&child, b"child").is_err());
}

#[test]
fn source_snapshot_requires_matching_retained_bytes() {
    let (_dir, store) = store();
    let source = b"full source";
    let snapshot = store.retain_exact_bytes(source).unwrap();
    let mut value = serde_json::to_value(evidence(b"preview")).unwrap();
    value["context"] = json!({"snapshot":{
        "sha256":format!("{:x}", Sha256::digest(source)), "bytes":source.len(),
        "captured_at_unix_ms":1, "artifact":snapshot
    }});
    let valid: Evidence = serde_json::from_value(value.clone()).unwrap();
    assert!(store.retain_evidence(&valid, b"preview").is_ok());
    value["context"]["snapshot"]["bytes"] = json!(source.len() + 1);
    let invalid: Evidence = serde_json::from_value(value.clone()).unwrap();
    assert!(store.retain_evidence(&invalid, b"preview").is_err());
    value["context"]["snapshot"]["bytes"] = json!(source.len());
    value["context"]["snapshot"]["sha256"] = json!("b".repeat(64));
    let invalid: Evidence = serde_json::from_value(value).unwrap();
    assert!(store.retain_evidence(&invalid, b"preview").is_err());
    std::fs::remove_file(store.path(&snapshot).unwrap()).unwrap();
    assert!(store.retain_evidence(&valid, b"preview").is_err());
}

#[cfg(unix)]
#[test]
fn evidence_reads_refuse_symlink_blobs() {
    let (dir, store) = store();
    let target = dir.path().join("target");
    std::fs::write(&target, b"{}").unwrap();
    let uri = format!("artifact://{:x}", Sha256::digest(b"{}"));
    std::os::unix::fs::symlink(target, store.path(&uri).unwrap()).unwrap();
    assert!(store.evidence_record(&uri).is_err());
}

#[test]
fn readable_manifests_use_the_canonical_encoding_discovery_recognizes() {
    let (_dir, store) = store();
    let citation = store.retain_evidence(&evidence(b"text"), b"text").unwrap();
    let record = store.evidence_record(&citation).unwrap();
    let pretty = serde_json::to_vec_pretty(&record).unwrap();
    let noncanonical = store.retain_exact_bytes(&pretty).unwrap();
    assert!(
        store
            .evidence_record(&noncanonical)
            .unwrap_err()
            .to_string()
            .contains("not canonical")
    );
    assert!(store.evidence_record(&citation).is_ok());
}

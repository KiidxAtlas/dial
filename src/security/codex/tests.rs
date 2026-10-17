use super::*;

#[test]
fn bundle_producer_version_is_carried_onto_the_scan() {
    let dir = std::env::temp_dir().join(format!("codex-bundle-version-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("scan-manifest.json"),
        r#"{"documentType":"codex-security.scan-manifest","schemaVersion":"1.0","scan":{"id":"s1","producer":{"name":"codex","version":"0.9.1"},"startedAt":"2024-01-01T00:00:00Z","completedAt":"2024-01-01T00:01:00Z","target":{"kind":"git_worktree"}}}"#,
    )
    .unwrap();
    fs::write(
        dir.join("findings.json"),
        r#"{"documentType":"codex-security.findings","schemaVersion":"1.0","scanId":"s1","findings":[]}"#,
    )
    .unwrap();
    fs::write(
        dir.join("coverage.json"),
        r#"{"documentType":"codex-security.coverage","schemaVersion":"1.0","scanId":"s1"}"#,
    )
    .unwrap();
    let result = import_bundle(&dir, &std::env::temp_dir(), "proj", "scan-1", None);
    fs::remove_dir_all(&dir).ok();
    let scan = result.unwrap();
    assert_eq!(scan.producer.name, "codex");
    assert_eq!(scan.producer.version.as_deref(), Some("0.9.1"));
}

#[test]
fn multibyte_location_path_does_not_panic() {
    assert_eq!(imported_location_path("a1€/x.rs").unwrap(), "a1€/x.rs");
    assert!(imported_location_path("C:/x.rs").is_err());
}

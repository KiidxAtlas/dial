use super::*;

#[test]
fn lexical_find_metadata_separates_complete_absence_from_capped_rows() {
    let (dir, ws) = crate::tools::testkit::workspace();
    fs::write(dir.path().join("a.rs"), "fn reconnect_network() {}\n").unwrap();
    fs::write(dir.path().join("b.rs"), "fn reconnect_network_again() {}\n").unwrap();

    let absent = find_result(&ws, &json!({"query":"zzz_absent_token"})).unwrap();
    assert!(
        absent.text.contains("No matching evidence."),
        "{}",
        absent.text
    );
    assert_eq!(absent.data["find"]["capped"], false, "{}", absent.data);
    assert_eq!(absent.data["find"]["shown"], json!([]), "{}", absent.data);
    assert_eq!(absent.data["find"]["complete"], true, "{}", absent.data);

    // A binary file is skipped, not searched: an empty result is then partial, not absent.
    fs::write(dir.path().join("blob.bin"), b"\0\0\x01reconnect\0").unwrap();
    let skipped = find_result(&ws, &json!({"query":"zzz_absent_token"})).unwrap();
    assert_eq!(skipped.data["find"]["complete"], false, "{}", skipped.data);
    fs::remove_file(dir.path().join("blob.bin")).unwrap();

    let capped = find_result(&ws, &json!({"query":"reconnect network", "limit":1})).unwrap();
    assert_eq!(capped.data["find"]["capped"], true, "{}", capped.data);
    assert_eq!(capped.data["find"]["complete"], false, "{}", capped.data);
    let shown = capped.data["find"]["shown"].as_array().unwrap();
    assert_eq!(shown.len(), 1, "{}", capped.data);
    // Structured revisions name the same rows the model reads, never a different digest.
    let sha = shown[0]["source_sha256"].as_str().unwrap();
    assert!(
        capped.text.contains(&format!("source_sha256={sha}")),
        "{}",
        capped.text
    );

    let full = find_result(&ws, &json!({"query":"reconnect network", "limit":10})).unwrap();
    assert_eq!(full.data["find"]["capped"], false, "{}", full.data);
    assert_eq!(
        full.data["find"]["shown"].as_array().unwrap().len(),
        2,
        "{}",
        full.data
    );
    assert_eq!(full.data["find"]["complete"], true, "{}", full.data);
}

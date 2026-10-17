use super::*;
#[test]
fn rejects_symlink_store_and_symlink_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let real = tempfile::tempdir().unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    assert!(ArtifactStore::new(&link).is_err());
    let store = ArtifactStore::new(dir.path()).unwrap();
    let uri = store.retain("evidence").unwrap();
    let blob = store.path(&uri).unwrap();
    fs::remove_file(&blob).unwrap();
    std::os::unix::fs::symlink(real.path().join("secret"), &blob).unwrap();
    assert!(store.retrieve(&uri, 1, 1).is_err());
    assert!(store.retrieve_bytes(&uri, 0, 100).is_err());
}
#[test]
fn byte_pages_recover_large_unbroken_binary_without_loss() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let bytes: Vec<u8> = (0..100_000).map(|n| (n % 256) as u8).collect();
    let mut spool = store.spool().unwrap();
    spool.write_all(&bytes).unwrap();
    let uri = spool.finish().unwrap();
    let mut recovered = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() as u64 {
        let value: Value =
            serde_json::from_str(&store.retrieve_bytes(&uri, offset, 8192).unwrap()).unwrap();
        recovered.extend(
            base64::engine::general_purpose::STANDARD
                .decode(value["data"].as_str().unwrap())
                .unwrap(),
        );
        offset = value["next_offset"].as_u64().unwrap();
    }
    assert_eq!(recovered, bytes);
}
#[test]
fn screenshots_become_compact_image_evidence_not_clipped_base64() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let png = b"\x89PNG\r\n\x1a\nreal-image-bytes";
    let envelope = json!({"artifact":{"media_type":"image/png","encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(png)}});
    let mut output = ToolOutput {
        execution: Default::default(),
        data: Some(envelope.clone()),
        text: envelope.to_string(),
        is_error: false,
    };
    let image = capture_image(Some(&store), &mut output).unwrap().unwrap();
    assert_eq!(
        fs::read(store.root.join(image["path"].as_str().unwrap())).unwrap(),
        png
    );
    assert!(output.text.contains("artifact://") && !output.text.contains("\"data\""));
}
#[test]
fn text_body_is_never_parsed_as_an_image_envelope() {
    use base64::Engine;
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    // A GitHub file whose body is JSON with a top-level image envelope: model-visible text
    // only (no structured data), so it must stay text and never be captured as an image.
    let body = json!({"artifact":{"media_type":"image/png","encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nhostile")}}).to_string();
    let mut output = ToolOutput {
        execution: Default::default(),
        data: None,
        text: body.clone(),
        is_error: false,
    };
    assert!(capture_image(Some(&store), &mut output).unwrap().is_none());
    assert_eq!(output.text, body);
    assert!(output.data.is_none());
}
#[test]
fn lifecycle_distinguishes_missing_recorded_blobs_from_unknown_references() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(dir.path()).unwrap();
    let uri = store.retain("evidence").unwrap();
    let info = store.info(&uri).unwrap();
    assert_eq!(info["state"], "retained");
    assert_eq!(info["retention"]["bytes"], 8);
    assert_eq!(
        store.call(&json!({"uri":uri,"info":true})).data.unwrap()["state"],
        "retained"
    );
    fs::remove_file(store.path(&uri).unwrap()).unwrap();
    assert_eq!(
        store.info(&uri).unwrap()["state"],
        "missing_after_retention"
    );
    assert!(
        store
            .retrieve(&uri, 1, 1)
            .unwrap_err()
            .to_string()
            .contains("Retention was recorded here")
    );
    let unknown = format!("artifact://{}", "f".repeat(64));
    assert_eq!(
        store.info(&unknown).unwrap()["state"],
        "no_record_in_this_store"
    );
    assert!(
        store
            .retrieve(&unknown, 1, 1)
            .unwrap_err()
            .to_string()
            .contains("No lifecycle record")
    );
    assert_eq!(store.retain("evidence").unwrap(), uri);
    assert_eq!(store.info(&uri).unwrap()["state"], "retained");
    fs::write(store.lifecycle_path(&uri).unwrap(), "not json").unwrap();
    assert!(
        store.info(&uri).is_err(),
        "corrupt metadata must not become invented history"
    );
}

use super::*;
#[test]
fn ephemeral_pixels_survive_deletion_and_can_be_promoted_to_durable_storage() {
    let source = tempfile::tempdir().unwrap();
    let pixels = b"\x89PNG\r\n\x1a\npixels";
    std::fs::write(source.path().join("source.png"), pixels).unwrap();
    let image =
        crate::llm::images::attachment(source.path(), std::path::Path::new("source.png")).unwrap();
    let mut message = json!({"role":"user", "dial_images":[image, null]});
    retain_volatile_message(&mut message);
    std::fs::remove_file(source.path().join("source.png")).unwrap();
    assert_eq!(
        crate::llm::images::verified_bytes(&message["dial_images"][0])
            .unwrap()
            .0,
        pixels
    );
    assert!(message["dial_images"][1].get("unavailable").is_some());
    let storage = tempfile::tempdir().unwrap();
    let store = crate::tools::artifact::ArtifactStore::new(storage.path()).unwrap();
    retain_message(&mut message, &store);
    assert!(
        message["dial_images"][0]["snapshot"]
            .get("inline")
            .is_none()
    );
    assert_eq!(
        crate::llm::images::verified_bytes(&message["dial_images"][0])
            .unwrap()
            .0,
        pixels
    );
}

use std::fs;
use std::sync::atomic::AtomicBool;

use super::{ensure_image_input, prepare_turn};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfixture";

#[test]
fn image_flag_is_refused_without_vision_support() {
    assert!(ensure_image_input(false, 1).is_err());
    assert!(ensure_image_input(true, 1).is_ok());
    assert!(ensure_image_input(false, 0).is_ok());
}

#[test]
fn explicit_image_is_attached_once_with_its_digest() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("shot.png");
    fs::write(&image, PNG).unwrap();
    let interrupt = AtomicBool::new(false);

    let turn = prepare_turn(
        dir.path(),
        "what is in this picture?",
        &[image.clone(), image],
        None,
        &interrupt,
    )
    .unwrap();

    assert_eq!(turn.text, "what is in this picture?");
    let attached = turn.message["dial_images"].as_array().unwrap();
    assert_eq!(attached.len(), 1);
    assert!(
        attached[0]["sha256"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty())
    );
}

#[test]
fn missing_image_file_fails_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let result = prepare_turn(
        dir.path(),
        "describe",
        &[dir.path().join("absent.png")],
        None,
        &interrupt,
    );
    assert!(result.is_err());
}

#[test]
fn stdin_image_is_detected_by_magic_bytes_and_verified() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let turn = prepare_turn(dir.path(), "describe", &[], Some(PNG), &interrupt).unwrap();
    let attached = turn.message["dial_images"].as_array().unwrap();
    assert_eq!(attached.len(), 1);
    let (bytes, mime) = crate::llm::images::verified_bytes(&attached[0]).unwrap();
    assert_eq!(bytes, PNG);
    assert_eq!(mime, "image/png");
}

#[test]
fn stdin_payload_that_is_not_an_image_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let interrupt = AtomicBool::new(false);
    let result = prepare_turn(dir.path(), "describe", &[], Some(b"just text"), &interrupt);
    assert!(result.is_err());
}

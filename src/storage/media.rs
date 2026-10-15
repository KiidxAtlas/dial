//! Own immutable image payloads independently of provider serialization.
use crate::llm::images::MAX_IMAGE_BYTES;
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::Write;

/// Transfer authorized images into immutable storage before journaling or inference. Source paths
/// remain provenance and authorization witnesses; only the snapshot supplies historical pixels.
/// A failed capture is recorded explicitly rather than silently substituting newer source bytes.
pub(crate) fn retain_message(message: &mut Value, store: &crate::tools::artifact::ArtifactStore) {
    retain(message, Some(store));
}

/// No-session conversations retain pixels in memory and never create storage.
pub(crate) fn retain_volatile_message(message: &mut Value) {
    retain(message, None);
}

fn retain(message: &mut Value, store: Option<&crate::tools::artifact::ArtifactStore>) {
    for reference in message
        .get_mut("dial_images")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if !reference.is_object() {
            *reference = json!({"unavailable":"invalid image reference"});
            continue;
        }
        if (reference.get("snapshot").is_some()
            && (store.is_none() || reference["snapshot"].get("inline").is_none()))
            || reference.get("unavailable").is_some()
        {
            continue;
        }
        let capture = (|| -> Result<Value> {
            let (bytes, _) = crate::llm::images::verified_bytes(reference)?;
            let Some(store) = store else {
                return Ok(json!({"sha256":reference["sha256"], "inline":STANDARD.encode(&bytes)}));
            };
            // Text-output truncation limits must never truncate binary media.
            let mut spool = store
                .clone()
                .with_limits(&crate::tools::artifact::ArtifactConfig {
                    max_bytes: MAX_IMAGE_BYTES,
                    head_bytes: MAX_IMAGE_BYTES,
                })
                .spool()?;
            spool.write_all(&bytes)?;
            let snapshot = store.image_reference(&spool.finish()?)?;
            ensure!(
                snapshot["sha256"] == reference["sha256"],
                "image snapshot digest changed"
            );
            Ok(snapshot)
        })();
        match capture {
            Ok(snapshot) => reference["snapshot"] = snapshot,
            Err(error) => {
                reference["unavailable"] = json!(format!("image capture failed: {error}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ephemeral_pixels_survive_deletion_and_can_be_promoted_to_durable_storage() {
        let source = tempfile::tempdir().unwrap();
        let pixels = b"\x89PNG\r\n\x1a\npixels";
        std::fs::write(source.path().join("source.png"), pixels).unwrap();
        let image =
            crate::llm::images::attachment(source.path(), std::path::Path::new("source.png"))
                .unwrap();
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
}

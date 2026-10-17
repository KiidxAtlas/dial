//! Own immutable image payloads independently of provider serialization.
use crate::llm::images::MAX_IMAGE_BYTES;
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::io::Write;

/// Transfer authorized images into immutable storage before journaling or inference. Source paths
/// remain provenance and authorization witnesses; only the snapshot supplies historical pixels.
/// A failed capture is recorded explicitly rather than silently substituting newer source bytes.
pub(crate) fn retain_message(message: &mut Value, store: &crate::storage::artifact::ArtifactStore) {
    retain(message, Some(store));
}

/// No-session conversations retain pixels in memory and never create storage.
pub(crate) fn retain_volatile_message(message: &mut Value) {
    retain(message, None);
}

fn retain(message: &mut Value, store: Option<&crate::storage::artifact::ArtifactStore>) {
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
                .with_limits(&crate::storage::artifact::ArtifactConfig {
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
mod tests;

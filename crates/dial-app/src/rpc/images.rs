//! RPC `image_settings`: the image route `image_gen` resolves now and the per-model generation
//! settings for it, read-only, so a client can draw the terminal's `/image settings` page.
//! Changes are written with `set_setting` on `image.models.<key>` (the whole entry, as the
//! terminal page saves it), then `apply_settings`.
use crate::config::image::{
    BACKGROUNDS, LORA_SCALE_LIMIT, MAX_STEPS, MODERATIONS, QUALITIES, SIZES,
};
use crate::config::types::Config;
use serde_json::{Value, json};

/// The route, or why there is none, plus the entry `/image settings` would edit for its model.
/// `settings` lists the `image.models` fields the route's API receives; values for other fields
/// are kept but not sent. A failing route reports `ok: false` and the error generation would give.
pub(super) fn settings(config: &Config) -> Value {
    let options = json!({
        "sizes": SIZES,
        "qualities": QUALITIES,
        "backgrounds": BACKGROUNDS,
        "moderations": MODERATIONS,
        "max_steps": MAX_STEPS,
        "lora_scale_limit": LORA_SCALE_LIMIT,
        "loras": crate::config::image::installed_loras(),
    });
    let role = config.role_selection("image").map(|selection| {
        json!({"provider": selection.provider, "model": selection.model, "base_url": selection.base_url})
    });
    match crate::tools::net::image_gen::current_route(config) {
        Ok(route) => {
            let (key, entry) = config.image.entry_for(&route.model);
            json!({
                "ok": true,
                "model": route.model,
                "api": route.api,
                "settings": route.settings,
                "edits": route.edits,
                "key": key,
                "entry": entry,
                "role": role,
                "options": options,
            })
        }
        Err(error) => json!({
            "ok": false,
            "error": format!("{error:#}"),
            "role": role,
            "options": options,
        }),
    }
}

/// Only immutable raster bytes from this project's engine-owned store; never a caller's path.
pub(super) fn artifact(root: &std::path::Path, params: &Value) -> anyhow::Result<Value> {
    use crate::storage::artifact::{ArtifactStore, artifact_id};
    use base64::{Engine, engine::general_purpose::STANDARD};
    use dial_protocol::ErrorCode;
    use sha2::{Digest, Sha256};

    let uri = params["uri"].as_str().unwrap_or_default();
    let id = artifact_id(uri).map_err(|e| super::coded(ErrorCode::InvalidParams, e))?;
    let bytes =
        ArtifactStore::new(root)?.exact_bytes(uri, crate::llm::images::MAX_IMAGE_BYTES as usize)?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == id,
        "image artifact changed"
    );
    let mime = crate::llm::images::sniff_mime(&bytes)
        .ok_or_else(|| super::coded(ErrorCode::InvalidParams, "artifact is not a raster image"))?;
    crate::llm::images::dimensions(&bytes)?;
    Ok(json!({"media_type":mime,"data":STANDARD.encode(bytes)}))
}

#[cfg(test)]
mod tests;

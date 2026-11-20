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

#[cfg(test)]
mod tests;

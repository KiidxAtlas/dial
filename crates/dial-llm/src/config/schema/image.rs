//! Image generation setting declarations. Defaults derive from typed configuration fields.
use super::*;

pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "image.auto",
        image.auto,
        TO,
        "Image generation",
        "Offer automatically",
        "Offer the image_gen tool whenever its endpoint (the [image] settings, or an image-capable session provider) has an API key, without listing it in integrations. Takes effect in a new session.",
        Kind::Bool,
        true,
    ),
    field!(
        "image.base_url",
        image.base_url,
        TO,
        "Image generation",
        "Endpoint",
        "OpenAI-compatible base URL used by the image_gen tool.",
        Kind::Text,
    ),
    field!(
        "image.model",
        image.model,
        TO,
        "Image generation",
        "Model",
        "Image model identifier sent with each image_gen request.",
        Kind::Text,
    ),
    field!(
        "image.timeout_secs",
        image.timeout_secs,
        TO,
        "Image generation",
        "Timeout",
        "Seconds to wait for one image generation. Zero disables the timeout.",
        Kind::Int {
            min: 0,
            max: 3600,
            step: 10,
        },
    ),
];

//! Image generation setting declarations. Defaults derive from typed configuration fields.
use super::*;

pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "image.auto",
        image.auto,
        TO,
        "Image generation",
        "Offer automatically",
        "Offer image_gen automatically when a shared image route resolves: image settings/role, main's provider, configured image profiles or keyed provider defaults. ChatGPT hosted-tool support may be unverified. Refreshes at request boundaries; calls still require approval.",
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

//! Image generation setting declarations. Defaults derive from typed configuration fields.
use super::*;

const IMAGE_APIS: EnumValues = &[
    ("", "Inferred from the endpoint"),
    ("images", "OpenAI-compatible images"),
    ("responses", "Responses image tool"),
    ("chat-completions", "Chat completions image output"),
    ("mlx-serve", "MLX-Serve images"),
    ("gemini", "Gemini native image output"),
    ("imagen", "Imagen on Vertex/compatible endpoint"),
    ("black-forest-labs", "Black Forest Labs"),
    ("comfyui", "ComfyUI workflow"),
    ("fal", "fal queue"),
    ("replicate", "Replicate predictions"),
    ("disabled", "Disabled"),
];

/// `image.api`: the accessor reports blank for unset so the schema default is a declared option.
const fn image_api_declaration() -> SettingDef {
    let mut declaration = def!(
        "image.api",
        TO,
        "Image generation",
        "Protocol",
        "Native image protocol for image.model. Blank infers it from the endpoint, as before.",
        Kind::Enum(IMAGE_APIS),
    );
    declaration.accessor = Some(|config| {
        Some(Value::String(crate::config::image::api_name(
            config.image.api,
        )))
    });
    declaration
}

pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "image.auto",
        image.auto,
        TO,
        "Image generation",
        "Offer automatically",
        "Offer image_gen automatically, independently of the chat model or an assigned image role. Missing automatic routes report setup requirements when called; explicit route errors remain visible. Calls still require a configured image service and approval. Refreshes at request boundaries.",
        Kind::Bool,
        true,
    ),
    field!(
        "image.base_url",
        image.base_url,
        TO,
        "Image generation",
        "Endpoint",
        "Image service base URL. The selected protocol determines generation and polling paths.",
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
    image_api_declaration(),
];

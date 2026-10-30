//! `[image]` route and per-model generation settings (`[image.models.<key>]`). Native protocols
//! are explicit; an unset protocol preserves OpenAI-compatible endpoint inference.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::profiles::ImageGenerationApi;

/// Output sizes `image_gen` accepts, per call or as a model's default.
pub const SIZES: &[&str] = &["auto", "1024x1024", "1536x1024", "1024x1536"];
/// `quality` values: gpt-image models take auto/low/medium/high, DALL·E 3 standard/hd.
pub const QUALITIES: &[&str] = &["auto", "low", "medium", "high", "standard", "hd"];
pub const BACKGROUNDS: &[&str] = &["auto", "transparent", "opaque"];
pub const MODERATIONS: &[&str] = &["auto", "low"];
pub const MAX_STEPS: u32 = 200;
pub const MAX_LORAS: usize = 8;
/// Bounds on an inline ComfyUI graph: real exported workflows are far below both.
pub const MAX_COMFY_NODES: usize = 512;
pub const MAX_COMFY_WORKFLOW_BYTES: usize = 1024 * 1024;
/// Most per-layer conditioning weights accepted; real text encoders tap 3 (FLUX) to 12 (Krea).
pub const MAX_COND_WEIGHTS: usize = 64;
/// LoRA scales outside this range are refused; real adapters use roughly 0..2.
pub const LORA_SCALE_LIMIT: f64 = 8.0;

/// `[image]` configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ImageConfig {
    /// Service base URL; the configured image protocol determines its API paths.
    pub base_url: String,
    /// Image model identifier sent with each generation request.
    pub model: String,
    /// Native image protocol for `model`. Unset keeps the legacy inference from `base_url`.
    #[serde(
        default,
        deserialize_with = "deserialize_api",
        skip_serializing_if = "Option::is_none"
    )]
    pub api: Option<ImageGenerationApi>,
    /// Environment variable holding the API key. Unset uses the provider's standard variable
    /// (`OPENAI_API_KEY` for api.openai.com) or a key saved through `/providers`.
    pub api_key_env: Option<String>,
    /// Per request timeout in seconds. Zero disables the timeout.
    pub timeout_secs: u64,
    /// Offer `image_gen` without an `integrations` entry whenever a shared image route resolves
    /// with its required credentials, independently of the main chat model's vision capability.
    pub auto: bool,
    /// Enable approved image_weights calls to scale floating-point checkpoint tensors into a
    /// separate file. Does not permit overwrites, bypass confinement or reload a running server.
    pub allow_weight_edits: bool,
    /// Generation settings per image model, matched by each entry's exact `model` identifier. Keys
    /// are free bare TOML names (`/image settings` derives one from the model).
    pub models: BTreeMap<String, ImageModelSettings>,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-image-1.5".into(),
            api: None,
            api_key_env: None,
            timeout_secs: 180,
            auto: true,
            allow_weight_edits: false,
            models: BTreeMap::new(),
        }
    }
}

impl ImageConfig {
    /// The settings declared for `model`, if any.
    pub fn settings_for(&self, model: &str) -> Option<&ImageModelSettings> {
        self.models
            .values()
            .find(|settings| settings.model == model)
    }

    /// The `image.models` key and settings `/image settings` edits for `model`: the existing
    /// entry, else an empty one under a key derived from the model (`image_<16 hex>`).
    pub fn entry_for(&self, model: &str) -> (String, ImageModelSettings) {
        use sha2::{Digest, Sha256};
        if let Some((key, settings)) = self.models.iter().find(|(_, s)| s.model == model) {
            return (key.clone(), settings.clone());
        }
        let settings = ImageModelSettings {
            model: model.to_owned(),
            ..ImageModelSettings::default()
        };
        (format!("image_{:.16x}", Sha256::digest(model)), settings)
    }

    /// # Errors
    /// Returns the first invalid or duplicate per-model entry.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for (key, settings) in &self.models {
            ensure!(
                seen.insert(settings.model.as_str()),
                "image.models.{key}: another entry already configures {:?}",
                settings.model
            );
            settings
                .validate()
                .map_err(|error| error.context(format!("image.models.{key}")))?;
        }
        Ok(())
    }
}

/// Blank means "infer from the endpoint", the same as an absent key.
fn deserialize_api<'de, D>(deserializer: D) -> Result<Option<ImageGenerationApi>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let raw: Option<String> = Option::deserialize(deserializer)?;
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(name) => serde_json::from_value(Value::String(name.to_owned()))
            .map(Some)
            .map_err(D::Error::custom),
    }
}

/// The settings-file name of `api` (`""` when unset), for schema accessors.
pub fn api_name(api: Option<ImageGenerationApi>) -> String {
    api.and_then(|api| serde_json::to_value(api).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Settings for one image model. Unset values leave the endpoint's own default. Each image API
/// receives only the settings it accepts (see `image_gen`); the rest are kept but not sent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ImageModelSettings {
    /// Exact model identifier these settings apply to.
    pub model: String,
    /// Default output size when a call does not choose one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// Diffusion steps (MLX-Serve).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    /// Fixed seed for repeatable images (MLX-Serve). Unset picks a random seed per image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// LoRA adapters applied in order (MLX-Serve).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub loras: Vec<ImageLora>,
    /// Rendering quality (OpenAI Images and Responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    /// Background transparency (OpenAI Images and Responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// Content moderation strictness (OpenAI Images and Responses).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation: Option<String>,
    /// How far an edit may move from its source image, 0–1 (MLX-Serve; used only for edits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strength: Option<f64>,
    /// Per-tapped-text-encoder-layer conditioning weights (MLX-Serve; 1 = neutral). The count is
    /// model specific (FLUX 3, Krea 12), so the server validates it. Empty leaves the model as is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cond_weights: Vec<f64>,
    /// Model-specific request fields for fal and Replicate (e.g. `aspect_ratio`). Adapter-owned
    /// prompt, size, output count and format fields cannot be overridden by these inputs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input: BTreeMap<String, Value>,
    /// ComfyUI API-format workflow (ComfyUI only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comfyui: Option<ComfyWorkflow>,
}

/// An inline ComfyUI API-format graph. Inline (not a workspace path) so reads stay within settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ComfyWorkflow {
    /// Node id → `{ "class_type": ..., "inputs": { ... } }`, as exported by ComfyUI's API format.
    pub workflow: Value,
    /// Node id whose `inputs.text` receives the prompt.
    pub prompt_node: String,
    /// Node id whose outputs hold the generated images.
    pub output_node: String,
}

impl ComfyWorkflow {
    /// # Errors
    /// Returns an error unless the graph is an object, both node ids are set, the prompt node has
    /// a string `inputs.text`, and the output node exists.
    pub fn validate(&self) -> Result<()> {
        let graph = self
            .workflow
            .as_object()
            .context("comfyui.workflow must be an object of API-format nodes")?;
        ensure!(
            graph.len() <= MAX_COMFY_NODES,
            "comfyui.workflow has more than {MAX_COMFY_NODES} nodes"
        );
        ensure!(
            serde_json::to_vec(&self.workflow)?.len() <= MAX_COMFY_WORKFLOW_BYTES,
            "comfyui.workflow is larger than {MAX_COMFY_WORKFLOW_BYTES} bytes"
        );
        ensure!(
            !self.prompt_node.trim().is_empty(),
            "comfyui.prompt_node must name a node id"
        );
        ensure!(
            !self.output_node.trim().is_empty(),
            "comfyui.output_node must name a node id"
        );
        let prompt = graph.get(&self.prompt_node).with_context(|| {
            format!(
                "comfyui.prompt_node {:?} is not in the workflow",
                self.prompt_node
            )
        })?;
        ensure!(
            prompt["inputs"]["text"].is_string(),
            "comfyui prompt node {:?} needs a string inputs.text",
            self.prompt_node
        );
        ensure!(
            graph.contains_key(&self.output_node),
            "comfyui.output_node {:?} is not in the workflow",
            self.output_node
        );
        Ok(())
    }
}

/// One LoRA adapter: a `.safetensors` file and its strength.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImageLora {
    /// Absolute path, or a file name inside MLX-Serve's LoRA directory (`~/.mlx-serve/models/loras`).
    pub path: String,
    #[serde(default = "default_lora_scale")]
    pub scale: f64,
}

fn default_lora_scale() -> f64 {
    1.0
}

impl ImageModelSettings {
    /// # Errors
    /// Returns the first value outside its documented range or option list.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.model.trim().is_empty(),
            "image model settings need the exact model identifier"
        );
        let one_of = |value: &Option<String>, options: &[&str], name: &str| -> Result<()> {
            if let Some(value) = value {
                ensure!(
                    options.contains(&value.as_str()),
                    "{name} must be one of {}",
                    options.join(", ")
                );
            }
            Ok(())
        };
        one_of(&self.size, SIZES, "size")?;
        one_of(&self.quality, QUALITIES, "quality")?;
        one_of(&self.background, BACKGROUNDS, "background")?;
        one_of(&self.moderation, MODERATIONS, "moderation")?;
        if let Some(steps) = self.steps {
            ensure!(
                (1..=MAX_STEPS).contains(&steps),
                "steps must be 1..{MAX_STEPS}"
            );
        }
        if let Some(seed) = self.seed {
            ensure!(i64::try_from(seed).is_ok(), "seed must be 0..{}", i64::MAX);
        }
        if let Some(strength) = self.strength {
            ensure!(
                strength.is_finite() && (0.0..=1.0).contains(&strength),
                "strength must be between 0 and 1"
            );
        }
        ensure!(
            self.cond_weights.len() <= MAX_COND_WEIGHTS
                && self.cond_weights.iter().all(|w| w.is_finite()),
            "cond_weights must be at most {MAX_COND_WEIGHTS} finite numbers"
        );
        ensure!(
            self.loras.len() <= MAX_LORAS,
            "at most {MAX_LORAS} LoRAs are allowed"
        );
        if let Some(workflow) = &self.comfyui {
            workflow.validate()?;
        }
        for lora in &self.loras {
            let path = lora.path.trim();
            ensure!(
                !path.is_empty() && path.ends_with(".safetensors"),
                "LoRA paths must name .safetensors files"
            );
            ensure!(
                lora.scale.is_finite() && lora.scale.abs() <= LORA_SCALE_LIMIT,
                "LoRA scale must be between -{LORA_SCALE_LIMIT} and {LORA_SCALE_LIMIT}"
            );
        }
        Ok(())
    }

    /// Whether anything beyond the model identifier is set.
    pub fn is_empty(&self) -> bool {
        Self {
            model: self.model.clone(),
            ..Self::default()
        } == *self
    }
}

/// MLX-Serve's LoRA directory, when a home directory is known.
pub fn mlx_serve_lora_dir() -> Option<PathBuf> {
    super::paths::home_dir().map(|home| home.join(".mlx-serve/models/loras"))
}

/// The file an `ImageLora::path` names: absolute paths as written, bare names in MLX-Serve's LoRA
/// directory.
pub fn lora_path(path: &str) -> PathBuf {
    let path = path.trim();
    let given = PathBuf::from(path);
    if given.is_absolute() {
        return given;
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = super::paths::home_dir()
    {
        return home.join(rest);
    }
    mlx_serve_lora_dir().map_or(given.clone(), |dir| dir.join(given))
}

/// Installed LoRA files in MLX-Serve's directory, by file name, sorted. Reads one directory only.
pub fn installed_loras() -> Vec<String> {
    let Some(dir) = mlx_serve_lora_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".safetensors") && !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests;

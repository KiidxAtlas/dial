//! `[image]` settings: the OpenAI-compatible endpoint used by the `image_gen` tool, and per-model
//! generation settings (`[image.models.<key>]`) applied to every request for that image model.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Output sizes `image_gen` accepts, per call or as a model's default.
pub const SIZES: &[&str] = &["auto", "1024x1024", "1536x1024", "1024x1536"];
/// `quality` values: gpt-image models take auto/low/medium/high, DALL·E 3 standard/hd.
pub const QUALITIES: &[&str] = &["auto", "low", "medium", "high", "standard", "hd"];
pub const BACKGROUNDS: &[&str] = &["auto", "transparent", "opaque"];
pub const MODERATIONS: &[&str] = &["auto", "low"];
pub const MAX_STEPS: u32 = 200;
pub const MAX_LORAS: usize = 8;
/// LoRA scales outside this range are refused; real adapters use roughly 0..2.
pub const LORA_SCALE_LIMIT: f64 = 8.0;

/// `[image]` configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ImageConfig {
    /// Base URL of an OpenAI-compatible API (`/images/generations`).
    pub base_url: String,
    /// Image model identifier sent with each generation request.
    pub model: String,
    /// Environment variable holding the API key. Unset uses the provider's standard variable
    /// (`OPENAI_API_KEY` for api.openai.com) or a key saved through `/providers`.
    pub api_key_env: Option<String>,
    /// Per request timeout in seconds. Zero disables the timeout.
    pub timeout_secs: u64,
    /// Offer `image_gen` without an `integrations` entry whenever a shared image route resolves
    /// with its required credentials, independently of the main chat model's vision capability.
    pub auto: bool,
    /// Generation settings per image model, matched by each entry's exact `model` identifier. Keys
    /// are free bare TOML names (`/image settings` derives one from the model).
    pub models: BTreeMap<String, ImageModelSettings>,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-image-1".into(),
            api_key_env: None,
            timeout_secs: 180,
            auto: true,
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
            self.loras.len() <= MAX_LORAS,
            "at most {MAX_LORAS} LoRAs are allowed"
        );
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

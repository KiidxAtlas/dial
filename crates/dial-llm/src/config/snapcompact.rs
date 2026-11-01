//! Snapcompact image-compaction settings and image billing rules (declarations only).

use crate::llm::identity::{Family, Identity};
use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;

pub const SHAPE_VARIANTS: &[&str] = &[
    "auto",
    "8x8r-bw",
    "8x8r-sent",
    "8x8u-bw",
    "8x8u-sent",
    "6x6u-bw",
    "6x6u-sent",
    "5x8-bw",
    "5x8-sent",
    "6x12-dim",
    "8x13-bw",
    "8on16-bw",
    "8on22-bw",
    "11on16-bw",
    "silver16-bw",
    "doc-8on16-bw",
    "doc-8on16-sent",
    "doc-8on16-sent-dim",
];

/// Inclusive pixel bounds for an explicit `frame_size` or `max_edge`.
pub const MIN_FRAME_SIZE: u32 = 64;
pub const MAX_FRAME_SIZE: u32 = 2576;
/// Baseline frame edge for explicit non-5x8 shapes and for auto shapes on ordinary models.
const DEFAULT_FRAME_SIZE: u32 = 1568;
/// Native edge of the 5x8 shapes.
const NATIVE_5X8_FRAME_SIZE: u32 = 2576;
/// Ideal edge for the Claude Opus 4.7+, Fable and Mythos families.
const LARGE_CLAUDE_FRAME_SIZE: u32 = 1932;
/// Ideal edge for Gemini models.
const GEMINI_FRAME_SIZE: u32 = 2048;

/// The wire family a request is sent through. Callers map the declared protocol to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapcompactFamily {
    Anthropic,
    Google,
    OpenAi,
    Unknown,
}

/// Image detail requested for each archive image. `auto` is resolved per model and wire family.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    #[default]
    Auto,
    High,
    Original,
}

impl ImageDetail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::High => "high",
            Self::Original => "original",
        }
    }

    /// Explicit hints are kept; auto uses original only on the supported GPT OpenAI tier.
    pub fn resolved_for_model(self, model: &str, family: SnapcompactFamily) -> Self {
        match self {
            Self::Auto
                if family == SnapcompactFamily::OpenAi
                    && Identity::new(model).gpt_5_4_or_later() =>
            {
                Self::Original
            }
            Self::Auto => Self::High,
            explicit => explicit,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemPromptMode {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "agents-md")]
    AgentsMd,
    #[serde(rename = "all")]
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct SnapcompactConfig {
    pub shape: String,
    pub system_prompt: SystemPromptMode,
    pub tool_results: bool,
    pub overlay: bool,
    /// Explicit frame edge in pixels; `None` (serialized `auto`) follows the shape and model.
    #[serde(serialize_with = "size_serialize")]
    pub frame_size: Option<u32>,
    pub image_detail: ImageDetail,
    /// Upper bound on the frame edge for every shape; `None` (serialized `auto`) is uncapped.
    #[serde(serialize_with = "size_serialize")]
    pub max_edge: Option<u32>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    shape: String,
    system_prompt: SystemPromptMode,
    tool_results: bool,
    overlay: bool,
    #[serde(deserialize_with = "size_deserialize")]
    frame_size: Option<u32>,
    image_detail: ImageDetail,
    #[serde(deserialize_with = "size_deserialize")]
    max_edge: Option<u32>,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            shape: "auto".into(),
            system_prompt: SystemPromptMode::None,
            tool_results: true,
            overlay: false,
            frame_size: None,
            image_detail: ImageDetail::Auto,
            max_edge: None,
        }
    }
}

/// Parses a pixel size or the canonical `auto` marker, as the compaction thresholds do.
fn size_deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<u32>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Size(u32),
        Marker(String),
    }
    match Input::deserialize(d)? {
        Input::Size(size) => Ok(Some(size)),
        Input::Marker(marker) if marker == "auto" => Ok(None),
        Input::Marker(_) => Err(serde::de::Error::custom(
            "expected a pixel size or the canonical 'auto' marker",
        )),
    }
}

fn size_serialize<S: Serializer>(
    value: &Option<u32>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(size) => size.serialize(s),
        None => s.serialize_str("auto"),
    }
}

impl TryFrom<RawConfig> for SnapcompactConfig {
    type Error = anyhow::Error;
    fn try_from(raw: RawConfig) -> Result<Self> {
        let config = Self {
            shape: raw.shape,
            system_prompt: raw.system_prompt,
            tool_results: raw.tool_results,
            overlay: raw.overlay,
            frame_size: raw.frame_size,
            image_detail: raw.image_detail,
            max_edge: raw.max_edge,
        };
        config.validate()?;
        Ok(config)
    }
}

impl Default for SnapcompactConfig {
    fn default() -> Self {
        RawConfig::default()
            .try_into()
            .expect("built-in snapcompact config is valid")
    }
}

impl SnapcompactConfig {
    pub fn validate(&self) -> Result<()> {
        if !SHAPE_VARIANTS.contains(&self.shape.as_str()) {
            bail!(
                "unsupported snapcompact shape {:?}; supported: {}",
                self.shape,
                SHAPE_VARIANTS.join(", ")
            );
        }
        for (name, size) in [("frame_size", self.frame_size), ("max_edge", self.max_edge)] {
            if let Some(size) = size
                && !(MIN_FRAME_SIZE..=MAX_FRAME_SIZE).contains(&size)
            {
                bail!(
                    "snapcompact {name} must be {MIN_FRAME_SIZE}..={MAX_FRAME_SIZE} pixels, got {size}"
                );
            }
        }
        Ok(())
    }

    /// Resolves `auto` settings for one request. The model name is matched by the identity owner;
    /// the wire family decides only what the model name does not declare. Explicit values are kept,
    /// and a resolved config re-resolves to itself, so persisted results cannot change later.
    pub fn resolved_for_model(&self, model: &str, family: SnapcompactFamily) -> Cow<'_, Self> {
        let identity = Identity::new(model);
        let known = identity.snapcompact_family();
        let explicit_shape = self.shape != "auto";
        let shape = if explicit_shape {
            self.shape.as_str()
        } else {
            auto_shape(known, family)
        };
        let default_size = if explicit_shape {
            if shape.starts_with("5x8") {
                NATIVE_5X8_FRAME_SIZE
            } else {
                DEFAULT_FRAME_SIZE
            }
        } else if identity.opus_4_7_or_later() || identity.fable_or_mythos() {
            LARGE_CLAUDE_FRAME_SIZE
        } else if known == Some(Family::Gemini) {
            GEMINI_FRAME_SIZE
        } else {
            DEFAULT_FRAME_SIZE
        };
        let size = self.frame_size.unwrap_or(default_size);
        let size = self.max_edge.map_or(size, |cap| size.min(cap));
        let image_detail = self.image_detail.resolved_for_model(model, family);
        let resolved = Self {
            shape: shape.to_owned(),
            system_prompt: self.system_prompt,
            tool_results: self.tool_results,
            overlay: self.overlay,
            frame_size: Some(size),
            image_detail,
            max_edge: self.max_edge,
        };
        if resolved == *self {
            Cow::Borrowed(self)
        } else {
            Cow::Owned(resolved)
        }
    }

    /// Compatibility wrapper for callers that know only whether the wire is Anthropic.
    pub fn resolved_for_provider(&self, anthropic: bool) -> Cow<'_, Self> {
        let family = if anthropic {
            SnapcompactFamily::Anthropic
        } else {
            SnapcompactFamily::Unknown
        };
        self.resolved_for_model("", family)
    }
}

/// The layout for a model identity when it is known, else the wire family's layout.
fn auto_shape(known: Option<Family>, family: SnapcompactFamily) -> &'static str {
    match known {
        Some(Family::Claude) => "11on16-bw",
        Some(Family::Gemini | Family::Gpt | Family::Kimi) => "8on22-bw",
        Some(Family::Glm) => "8on16-bw",
        _ if family == SnapcompactFamily::Anthropic => "11on16-bw",
        _ => "8on22-bw",
    }
}

#[cfg(test)]
mod tests;

/// Explicit model/operator image billing declarations. No resizing, patch caps,
/// model-name inference, or gateway capability inference is hidden in these rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageTokenization {
    #[default]
    PixelCeiling,
    Fixed {
        tokens: u64,
    },
    Patches {
        patch_size: u32,
        tokens_per_patch: u32,
        base_tokens: u64,
    },
    Area {
        pixels_per_token: u32,
        base_tokens: u64,
    },
}

impl ImageTokenization {
    pub fn validate(&self) -> Result<()> {
        match *self {
            Self::PixelCeiling => {}
            Self::Fixed { tokens } if (1..=100_000_000).contains(&tokens) => {}
            Self::Patches {
                patch_size,
                tokens_per_patch,
                base_tokens,
            } if (1..=4096).contains(&patch_size)
                && (1..=1_000_000).contains(&tokens_per_patch)
                && base_tokens <= 100_000_000 => {}
            Self::Area {
                pixels_per_token,
                base_tokens,
            } if (1..=1_000_000).contains(&pixels_per_token) && base_tokens <= 100_000_000 => {}
            _ => bail!("invalid explicit snapcompact image-tokenization rule"),
        }
        Ok(())
    }
    pub fn token_upper_bound(&self, width: u32, height: u32) -> Result<u64> {
        self.validate()?;
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            bail!("image dimensions must be 1..=16384 pixels per edge");
        }
        let pixels = u64::from(width) * u64::from(height);
        Ok(match *self {
            Self::PixelCeiling => pixels,
            Self::Fixed { tokens } => tokens,
            Self::Patches {
                patch_size,
                tokens_per_patch,
                base_tokens,
            } => {
                u64::from(width.div_ceil(patch_size))
                    * u64::from(height.div_ceil(patch_size))
                    * u64::from(tokens_per_patch)
                    + base_tokens
            }
            Self::Area {
                pixels_per_token,
                base_tokens,
            } => pixels.div_ceil(u64::from(pixels_per_token)) + base_tokens,
        })
    }
}

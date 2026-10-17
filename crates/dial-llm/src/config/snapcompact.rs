//! Snapcompact image-compaction settings and image billing rules (declarations only).

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

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
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    shape: String,
    system_prompt: SystemPromptMode,
    tool_results: bool,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            shape: "auto".into(),
            system_prompt: SystemPromptMode::None,
            tool_results: true,
        }
    }
}

impl TryFrom<RawConfig> for SnapcompactConfig {
    type Error = anyhow::Error;
    fn try_from(raw: RawConfig) -> Result<Self> {
        let config = Self {
            shape: raw.shape,
            system_prompt: raw.system_prompt,
            tool_results: raw.tool_results,
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
        Ok(())
    }
    /// Auto is resolved from the actual declared wire family, never from a guessed model name.
    pub fn resolved_for_provider(&self, anthropic: bool) -> std::borrow::Cow<'_, Self> {
        if self.shape != "auto" {
            return std::borrow::Cow::Borrowed(self);
        }
        std::borrow::Cow::Owned(Self {
            shape: if anthropic { "11on16-bw" } else { "8on22-bw" }.into(),
            system_prompt: self.system_prompt,
            tool_results: self.tool_results,
        })
    }
}

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

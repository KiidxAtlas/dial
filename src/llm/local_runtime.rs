//! Speculative decoding and KV-cache controls for Dial-owned MLX-LM servers.
//!
//! Each field maps onto one flag of the pinned `mlx_lm.server`. Unset fields are
//! omitted from the command line so the server keeps its own defaults. In settings
//! and TOML, `"auto"` (and an empty draft model) means unset.

use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};

/// `mlx_lm.server --num-draft-tokens` default, used when a draft model is set without a count.
pub const DEFAULT_NUM_DRAFT_TOKENS: u32 = 3;
pub const MAX_NUM_DRAFT_TOKENS: u32 = 16;
/// Quantization widths MLX supports for affine-quantized tensors.
pub const KV_BITS: &[u8] = &[2, 3, 4, 6, 8];
/// Quantization group sizes MLX supports for affine-quantized tensors.
pub const KV_GROUP_SIZES: &[u32] = &[32, 64, 128];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalRuntime {
    /// Draft model id or local path for speculative decoding; `""` or `"auto"` disables it.
    #[serde(
        deserialize_with = "blank_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub draft_model: Option<String>,
    #[serde(
        deserialize_with = "auto_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub num_draft_tokens: Option<u32>,
    #[serde(
        deserialize_with = "auto_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub kv_bits: Option<u8>,
    #[serde(
        deserialize_with = "auto_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub kv_group_size: Option<u32>,
    #[serde(
        deserialize_with = "auto_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub quantized_kv_start: Option<u32>,
}

impl LocalRuntime {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Rejects values and combinations the pinned mlx-lm server cannot honor.
    pub fn validate(&self) -> Result<()> {
        if let Some(draft) = &self.draft_model {
            ensure!(
                draft == draft.trim() && !draft.is_empty(),
                "draft_model must be a model id or path without surrounding whitespace"
            );
        }
        if let Some(count) = self.num_draft_tokens {
            ensure!(
                self.draft_model.is_some(),
                "num_draft_tokens requires draft_model"
            );
            ensure!(
                (1..=MAX_NUM_DRAFT_TOKENS).contains(&count),
                "num_draft_tokens must be between 1 and {MAX_NUM_DRAFT_TOKENS}, got {count}"
            );
        }
        match self.kv_bits {
            Some(bits) => ensure!(
                KV_BITS.contains(&bits),
                "kv_bits must be one of {KV_BITS:?}, got {bits}"
            ),
            None => {
                ensure!(
                    self.kv_group_size.is_none(),
                    "kv_group_size requires kv_bits (KV-cache quantization)"
                );
                ensure!(
                    self.quantized_kv_start.is_none(),
                    "quantized_kv_start requires kv_bits (KV-cache quantization)"
                );
            }
        }
        if let Some(size) = self.kv_group_size {
            ensure!(
                KV_GROUP_SIZES.contains(&size),
                "kv_group_size must be one of {KV_GROUP_SIZES:?}, got {size}"
            );
        }
        Ok(())
    }

    /// Server flags for this runtime. Callers must `validate` first.
    pub fn server_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(draft) = &self.draft_model {
            args.push(format!("--draft-model={draft}"));
            let count = self.num_draft_tokens.unwrap_or(DEFAULT_NUM_DRAFT_TOKENS);
            args.push(format!("--num-draft-tokens={count}"));
        }
        if let Some(bits) = self.kv_bits {
            args.push(format!("--kv-bits={bits}"));
            if let Some(size) = self.kv_group_size {
                args.push(format!("--kv-group-size={size}"));
            }
            if let Some(start) = self.quantized_kv_start {
                args.push(format!("--quantized-kv-start={start}"));
            }
        }
        args
    }
}

fn blank_as_none<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.filter(|text| !text.trim().is_empty() && !text.trim().eq_ignore_ascii_case("auto")))
}

fn auto_as_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input<T> {
        Value(T),
        Text(String),
    }
    match Input::<T>::deserialize(deserializer)? {
        Input::Value(value) => Ok(Some(value)),
        Input::Text(text) if text.eq_ignore_ascii_case("auto") => Ok(None),
        Input::Text(_) => Err(serde::de::Error::custom("expected an integer or 'auto'")),
    }
}

#[cfg(test)]
mod tests;

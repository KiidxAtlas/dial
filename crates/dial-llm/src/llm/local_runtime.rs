//! Speculative decoding and KV-cache controls for Dial-owned MLX-LM servers.
//!
//! Each field maps onto one flag of the pinned `mlx_lm.server`. Unset fields are
//! omitted from the command line so the server keeps its own defaults. In settings
//! and TOML, `"auto"` (and an empty draft model) means unset.

use anyhow::{Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};

mod gguf;
pub use gguf::{GgufEngines, GgufLoader};

/// `mlx_lm.server --num-draft-tokens` default, used when a draft model is set without a count.
pub const DEFAULT_NUM_DRAFT_TOKENS: u32 = 3;
pub const MAX_NUM_DRAFT_TOKENS: u32 = 16;
/// Quantization widths MLX supports for affine-quantized tensors.
pub const KV_BITS: &[u8] = &[2, 3, 4, 6, 8];
/// Quantization group sizes MLX supports for affine-quantized tensors.
pub const KV_GROUP_SIZES: &[u32] = &[32, 64, 128];

/// A choice of executable, independent of sampling and model format. Auto preserves Dial's
/// original format routing. A pin is never silently replaced by a different runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalEngine {
    #[default]
    Auto,
    Mlx,
    MlxServe,
    VllmMlx,
    LlamaCpp,
    Atomic,
    IkLlama,
    MistralRs,
    Vllm,
    Sglang,
    Exl3,
}

impl LocalEngine {
    pub fn model_key(model: &str) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(model.as_bytes()))
    }

    /// Stable kebab-case identifier matching serde and RPC wire format.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Mlx => "mlx",
            Self::MlxServe => "mlx-serve",
            Self::VllmMlx => "vllm-mlx",
            Self::LlamaCpp => "llama-cpp",
            Self::Atomic => "atomic",
            Self::IkLlama => "ik-llama",
            Self::MistralRs => "mistral-rs",
            Self::Vllm => "vllm",
            Self::Sglang => "sglang",
            Self::Exl3 => "exl3",
        }
    }

    /// Human-readable label for UI pickers and diagnostics.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic",
            Self::Mlx => "MLX",
            Self::MlxServe => "MLX-Serve",
            Self::VllmMlx => "vLLM-MLX",
            Self::LlamaCpp => "llama.cpp",
            Self::Atomic => "Atomic Engine",
            Self::IkLlama => "ik_llama.cpp",
            Self::MistralRs => "mistral.rs",
            Self::Vllm => "vLLM",
            Self::Sglang => "SGLang",
            Self::Exl3 => "ExLlamaV3",
        }
    }

    /// Dependency recipe id in `dial_tools::setup::deps::RECIPES` (for `/setup <id>` / `doctor --fix`).
    pub const fn recipe_id(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Mlx => Some("mlx-runtime"),
            Self::MlxServe => Some("mlx-serve"),
            Self::VllmMlx => Some("vllm-mlx"),
            Self::LlamaCpp => Some("llama-cpp"),
            Self::Atomic => Some("atomic-engine"),
            Self::IkLlama => Some("ik-llama"),
            Self::MistralRs => Some("mistral-rs"),
            Self::Vllm => Some("vllm"),
            Self::Sglang => Some("sglang"),
            Self::Exl3 => Some("exl3"),
        }
    }

    /// Runtime that `Auto` selects on this platform for `(format, gguf)`.
    pub fn default_for(
        format: super::local_format::LocalFormat,
        gguf: GgufEngines,
    ) -> Option<Self> {
        use super::local_format::LocalFormat;
        let apple = cfg!(all(target_os = "macos", target_arch = "aarch64"));
        match (format, gguf) {
            (LocalFormat::Mlx, _) if apple => Some(Self::Mlx),
            (LocalFormat::Mlx, _) if cfg!(target_os = "linux") => {
                if super::local_format::vllm_server().is_some() {
                    Some(Self::Vllm)
                } else if super::local_format::sglang_server().is_some() {
                    Some(Self::Sglang)
                } else if super::local_format::mistralrs_server().is_some() {
                    Some(Self::MistralRs)
                } else {
                    Some(Self::Vllm)
                }
            }
            (LocalFormat::Mlx, _) => Some(Self::MistralRs),
            (LocalFormat::Gguf, GgufEngines::AtomicOnly) if apple => Some(Self::Atomic),
            (LocalFormat::Gguf, GgufEngines::Both) => Some(Self::LlamaCpp),
            (LocalFormat::Exl3, _) if !cfg!(target_os = "macos") => Some(Self::Exl3),
            _ => None,
        }
    }

    /// Whether this runtime's local executable/environment is currently installed on disk.
    pub fn is_installed(self, user_dir: &std::path::Path) -> bool {
        use super::local_format;
        match self {
            Self::Auto => true,
            Self::Mlx => local_format::mlx_python_ready(user_dir),
            Self::MlxServe => local_format::mlx_serve().is_some(),
            Self::VllmMlx => local_format::vllm_mlx_server().is_some(),
            Self::LlamaCpp => local_format::llama_server().is_some(),
            Self::Atomic => local_format::atomic_server().is_some(),
            Self::IkLlama => local_format::ik_llama_server().is_some(),
            Self::MistralRs => local_format::mistralrs_server().is_some(),
            Self::Vllm => local_format::vllm_server().is_some(),
            Self::Sglang => local_format::sglang_server().is_some(),
            Self::Exl3 => local_format::exl3_server().is_some(),
        }
    }

    pub fn choices(format: super::local_format::LocalFormat, gguf: GgufEngines) -> &'static [Self] {
        use super::local_format::LocalFormat;
        let apple = cfg!(all(target_os = "macos", target_arch = "aarch64"));
        match (format, gguf) {
            (LocalFormat::Mlx, _) if apple => &[
                Self::Auto,
                Self::Mlx,
                Self::MlxServe,
                Self::VllmMlx,
                Self::MistralRs,
            ],
            (LocalFormat::Mlx, _) if cfg!(target_os = "linux") => {
                &[Self::Auto, Self::Vllm, Self::Sglang, Self::MistralRs]
            }
            (LocalFormat::Mlx, _) => &[Self::Auto, Self::MistralRs],
            (LocalFormat::Gguf, GgufEngines::AtomicOnly) if apple => &[Self::Atomic],
            (LocalFormat::Gguf, GgufEngines::Both) if apple => &[
                Self::Auto,
                Self::LlamaCpp,
                Self::Atomic,
                Self::IkLlama,
                Self::MistralRs,
            ],
            (LocalFormat::Gguf, GgufEngines::Both) => {
                &[Self::Auto, Self::LlamaCpp, Self::IkLlama, Self::MistralRs]
            }
            (LocalFormat::Exl3, _) if !cfg!(target_os = "macos") => &[Self::Auto, Self::Exl3],
            _ => &[],
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalRuntime {
    /// Resolved by the backend from the selected model, not inherited as a global tuning flag.
    #[serde(skip)]
    pub engine: LocalEngine,
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
        self.draft_model.is_none()
            && self.num_draft_tokens.is_none()
            && self.kv_bits.is_none()
            && self.kv_group_size.is_none()
            && self.quantized_kv_start.is_none()
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

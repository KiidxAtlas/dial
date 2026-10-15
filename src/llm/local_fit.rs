//! Pure memory-fit estimates for local MLX checkpoints (`dial models recommend`).
//!
//! These are planning heuristics, not measurements: weights are the on-disk safetensors
//! size, the KV cache follows the attention layout declared in `config.json`, and a fixed
//! runtime overhead is added. Nothing here loads a model.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde::Serialize;
use serde_json::Value;

const GIB: u64 = 1 << 30;
/// Resident fraction of physical memory treated as comfortable for local serving.
pub const COMFORTABLE_FRACTION: f64 = 0.60;
/// Resident fraction up to which a model is tight; beyond it the model will not fit.
pub const LIMIT_FRACTION: f64 = 0.75;
const FIXED_OVERHEAD_BYTES: u64 = 3 * GIB / 2;
const WEIGHT_OVERHEAD_PERCENT: u64 = 5;
/// Group size assumed for quantized KV caches; each group stores fp16 scale and bias.
const QUANT_GROUP_SIZE: f64 = 64.0;
const DEFAULT_MAX_CONTEXT: u64 = 1 << 20;

/// Fit verdict against physical memory. Variant order is the ranking order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    Fits,
    Tight,
    WontFit,
    Unknown,
}

/// Classify a resident footprint against physical memory. Unknown memory yields `Unknown`.
pub fn classify(resident_bytes: u64, memory_bytes: Option<u64>) -> Fit {
    let Some(memory) = memory_bytes.filter(|m| *m > 0) else {
        return Fit::Unknown;
    };
    let resident = resident_bytes as f64;
    let memory = memory as f64;
    if resident <= memory * COMFORTABLE_FRACTION {
        Fit::Fits
    } else if resident <= memory * LIMIT_FRACTION {
        Fit::Tight
    } else {
        Fit::WontFit
    }
}

/// Parses `sysctl -n hw.memsize` output (bytes).
pub fn parse_sysctl_memsize(output: &str) -> Option<u64> {
    output.trim().parse::<u64>().ok().filter(|bytes| *bytes > 0)
}

/// Parses the `MemTotal` line of `/proc/meminfo` (kB) into bytes.
pub fn parse_meminfo_total(text: &str) -> Option<u64> {
    let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024).filter(|bytes| *bytes > 0)
}

/// Physical memory in bytes on macOS and Linux; `None` elsewhere or when unreadable.
pub fn physical_memory_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let output = Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        return parse_sysctl_memsize(&String::from_utf8_lossy(&output.stdout));
    }
    if cfg!(target_os = "linux") {
        return parse_meminfo_total(&fs::read_to_string("/proc/meminfo").ok()?);
    }
    None
}

/// Attention layout relevant to KV-cache size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Architecture {
    /// Layers with full-context KV cache.
    pub full_layers: u64,
    /// Layers with sliding-window KV cache.
    pub windowed_layers: u64,
    /// Sliding window in tokens (meaningful only when `windowed_layers > 0`).
    pub window: u64,
    /// Cached scalar values per token per layer (K and V, or MLA latent plus rope).
    pub values_per_token_per_layer: u64,
    /// Native maximum position count from the config, when declared.
    pub max_positions: Option<u64>,
}

fn uint(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64()
}

impl Architecture {
    /// Reads the attention layout from a Hugging Face `config.json`.
    pub fn from_config(config: &Value) -> Option<Self> {
        let text = config
            .get("text_config")
            .filter(|value| value.is_object())
            .unwrap_or(config);
        let layers = uint(text, "num_hidden_layers")?;
        let values_per_token_per_layer = if let Some(rank) = uint(text, "kv_lora_rank") {
            rank + uint(text, "qk_rope_head_dim").unwrap_or(0)
        } else {
            let heads = uint(text, "num_attention_heads").filter(|heads| *heads > 0)?;
            let kv_heads = uint(text, "num_key_value_heads").unwrap_or(heads);
            let head_dim = match uint(text, "head_dim") {
                Some(dim) => dim,
                None => uint(text, "hidden_size")? / heads,
            };
            2 * kv_heads * head_dim
        };
        let window = uint(text, "sliding_window").unwrap_or(0);
        let (mut full_layers, mut windowed_layers) = (layers, 0);
        if let Some(kinds) = text
            .get("layer_types")
            .and_then(Value::as_array)
            .filter(|kinds| kinds.len() as u64 == layers)
        {
            full_layers = 0;
            for kind in kinds.iter().map(|kind| kind.as_str().unwrap_or("")) {
                if kind.contains("linear") || kind.contains("mamba") || kind.contains("ssm") {
                    // Recurrent layers keep no per-token KV cache.
                } else if kind.contains("sliding") && window > 0 {
                    windowed_layers += 1;
                } else {
                    full_layers += 1;
                }
            }
        }
        Some(Self {
            full_layers,
            windowed_layers,
            window,
            values_per_token_per_layer,
            max_positions: uint(text, "max_position_embeddings"),
        })
    }
}

/// Quantization bits declared by `quantization` or `quantization_config`, if any.
pub fn quantization_bits(config: &Value) -> Option<u64> {
    ["quantization", "quantization_config"]
        .iter()
        .find_map(|key| config.get(*key)?.get("bits")?.as_u64())
}

/// Total size of `*.safetensors` files in a snapshot directory (symlinks followed).
/// `None` when the directory is unreadable or has no weight files.
pub fn safetensors_bytes(dir: &Path) -> Option<u64> {
    let mut total = 0u64;
    let mut found = false;
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("safetensors") {
            continue;
        }
        total += fs::metadata(&path).ok()?.len();
        found = true;
    }
    found.then_some(total)
}

/// KV-cache bytes for `context` tokens. `kv_bits` of `None` means fp16 cache.
pub fn kv_bytes(arch: &Architecture, context: u64, kv_bits: Option<u8>) -> u64 {
    let bytes_per_value = match kv_bits {
        None => 2.0,
        Some(bits) => f64::from(bits) / 8.0 + 4.0 / QUANT_GROUP_SIZE,
    };
    let full_tokens = context as f64 * arch.full_layers as f64;
    let windowed_tokens = context.min(arch.window) as f64 * arch.windowed_layers as f64;
    ((full_tokens + windowed_tokens) * arch.values_per_token_per_layer as f64 * bytes_per_value)
        .round() as u64
}

/// Fixed runtime overhead plus a share of the weights.
pub fn overhead_bytes(weights_bytes: u64) -> u64 {
    FIXED_OVERHEAD_BYTES + weights_bytes * WEIGHT_OVERHEAD_PERCENT / 100
}

/// Total resident footprint for a context length.
pub fn resident_bytes(
    weights_bytes: u64,
    arch: &Architecture,
    context: u64,
    kv_bits: Option<u8>,
) -> u64 {
    weights_bytes + kv_bytes(arch, context, kv_bits) + overhead_bytes(weights_bytes)
}

/// Context actually used: the requested length capped by the native maximum when known.
pub fn effective_context(requested: u64, arch: &Architecture) -> u64 {
    arch.max_positions
        .map_or(requested, |max| requested.min(max))
}

/// Estimate for one checkpoint at a requested context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Estimate {
    pub context_tokens: u64,
    pub weights_bytes: u64,
    pub kv_bytes: u64,
    pub overhead_bytes: u64,
    pub resident_bytes: u64,
    pub fit: Fit,
}

/// Estimates the resident footprint and fit verdict at the effective context.
pub fn estimate(
    weights_bytes: u64,
    arch: &Architecture,
    requested_context: u64,
    kv_bits: Option<u8>,
    memory_bytes: Option<u64>,
) -> Estimate {
    let context_tokens = effective_context(requested_context, arch);
    let kv = kv_bytes(arch, context_tokens, kv_bits);
    let overhead = overhead_bytes(weights_bytes);
    let resident = weights_bytes + kv + overhead;
    Estimate {
        context_tokens,
        weights_bytes,
        kv_bytes: kv,
        overhead_bytes: overhead,
        resident_bytes: resident,
        fit: classify(resident, memory_bytes),
    }
}

/// Largest context whose resident footprint stays within the comfortable budget.
/// Returns 0 when memory is unknown or even one token does not fit.
pub fn max_context(
    weights_bytes: u64,
    arch: &Architecture,
    kv_bits: Option<u8>,
    memory_bytes: Option<u64>,
) -> u64 {
    let Some(memory) = memory_bytes.filter(|m| *m > 0) else {
        return 0;
    };
    let budget = (memory as f64 * COMFORTABLE_FRACTION) as u64;
    let cap = arch.max_positions.unwrap_or(DEFAULT_MAX_CONTEXT);
    let fits = |context: u64| resident_bytes(weights_bytes, arch, context, kv_bits) <= budget;
    if cap == 0 || !fits(1) {
        return 0;
    }
    let (mut low, mut high) = (1u64, cap);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn arch(full_layers: u64, values: u64) -> Architecture {
        Architecture {
            full_layers,
            windowed_layers: 0,
            window: 0,
            values_per_token_per_layer: values,
            max_positions: None,
        }
    }

    #[test]
    fn classify_uses_comfortable_and_limit_thresholds() {
        let memory = Some(100 * GIB);
        assert_eq!(classify(50 * GIB, memory), Fit::Fits);
        assert_eq!(classify(70 * GIB, memory), Fit::Tight);
        assert_eq!(classify(80 * GIB, memory), Fit::WontFit);
        assert_eq!(classify(1, None), Fit::Unknown);
        assert_eq!(classify(1, Some(0)), Fit::Unknown);
    }

    #[test]
    fn parses_physical_memory_sources() {
        assert_eq!(parse_sysctl_memsize("17179869184\n"), Some(16 * GIB));
        assert_eq!(parse_sysctl_memsize("0"), None);
        assert_eq!(parse_sysctl_memsize("not a number"), None);
        let meminfo = "MemFree: 1 kB\nMemTotal:       8388608 kB\n";
        assert_eq!(parse_meminfo_total(meminfo), Some(8 * GIB));
        assert_eq!(parse_meminfo_total("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn full_attention_kv_is_fp16_by_default_and_shrinks_with_bits() {
        // 2 layers * 1000 tokens * 256 values * 2 bytes.
        assert_eq!(kv_bytes(&arch(2, 256), 1000, None), 1_024_000);
        // 4-bit: 0.5 bytes plus 4/64 bytes of group scale and bias per value.
        assert_eq!(kv_bytes(&arch(2, 256), 1000, Some(4)), 288_000);
    }

    #[test]
    fn sliding_layers_cap_at_window() {
        let sliding = Architecture {
            full_layers: 0,
            windowed_layers: 4,
            window: 512,
            values_per_token_per_layer: 100,
            max_positions: None,
        };
        // 4 layers * min(2000, 512) tokens * 100 values * 2 bytes.
        assert_eq!(kv_bytes(&sliding, 2000, None), 409_600);
    }

    #[test]
    fn architecture_reads_hybrid_layer_types_from_text_config() {
        let config = json!({
            "text_config": {
                "num_hidden_layers": 4,
                "layer_types": ["sliding_attention", "full_attention", "linear_attention", "full_attention"],
                "sliding_window": 128,
                "num_attention_heads": 8,
                "num_key_value_heads": 2,
                "head_dim": 64,
                "max_position_embeddings": 4096
            }
        });
        let parsed = Architecture::from_config(&config).expect("architecture");
        assert_eq!(parsed.full_layers, 2);
        assert_eq!(parsed.windowed_layers, 1);
        assert_eq!(parsed.window, 128);
        assert_eq!(parsed.values_per_token_per_layer, 256);
        assert_eq!(parsed.max_positions, Some(4096));
    }

    #[test]
    fn architecture_uses_mla_latent_width() {
        let config = json!({
            "num_hidden_layers": 3,
            "num_attention_heads": 8,
            "kv_lora_rank": 512,
            "qk_rope_head_dim": 64
        });
        let parsed = Architecture::from_config(&config).expect("architecture");
        assert_eq!(parsed.full_layers, 3);
        assert_eq!(parsed.values_per_token_per_layer, 576);
        assert_eq!(parsed.max_positions, None);
    }

    #[test]
    fn architecture_requires_layer_count() {
        assert!(Architecture::from_config(&json!({"hidden_size": 64})).is_none());
    }

    #[test]
    fn max_context_is_zero_when_weights_exceed_budget() {
        let memory = Some(16 * GIB);
        assert_eq!(max_context(10 * GIB, &arch(2, 256), None, memory), 0);
        assert_eq!(max_context(GIB, &arch(2, 256), None, None), 0);
    }

    #[test]
    fn max_context_is_the_exact_budget_boundary() {
        let weights = 2 * GIB;
        let memory = Some(16 * GIB);
        let layout = Architecture {
            max_positions: Some(4_000_000),
            ..arch(2, 1024)
        };
        let budget = (16 * GIB) as f64 * COMFORTABLE_FRACTION;
        let found = max_context(weights, &layout, None, memory);
        assert!(found > 0 && found < 4_000_000);
        assert!(resident_bytes(weights, &layout, found, None) as f64 <= budget);
        assert!(resident_bytes(weights, &layout, found + 1, None) as f64 > budget);
    }

    #[test]
    fn estimate_caps_requested_context_at_native_maximum() {
        let layout = Architecture {
            max_positions: Some(4096),
            ..arch(2, 256)
        };
        let result = estimate(GIB, &layout, 32768, None, Some(16 * GIB));
        assert_eq!(result.context_tokens, 4096);
        assert_eq!(result.fit, Fit::Fits);
        assert_eq!(
            result.resident_bytes,
            result.weights_bytes + result.kv_bytes + result.overhead_bytes
        );
    }
}

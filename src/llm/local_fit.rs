//! Pure memory-fit estimates for local models (`dial models recommend`, GGUF serving context).
//!
//! These are planning heuristics, not measurements: weights are the on-disk safetensors or GGUF
//! size, the KV cache follows the attention layout declared in `config.json` or the GGUF header,
//! and a fixed runtime overhead is added. Nothing here loads a model.

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

    /// Reads the attention layout from GGUF header metadata. Sliding-window patterns are not
    /// described there, so those layers count as full attention: an upper bound on the cache.
    /// Hybrid models that declare `full_attention_interval` keep KV only on those layers.
    pub fn from_gguf(info: &crate::llm::local_format::GgufInfo) -> Option<Self> {
        let layers = info.block_count.filter(|layers| *layers > 0)?;
        let values_per_token_per_layer = if let Some(rank) = info.kv_lora_rank {
            rank + info.rope_dimension_count.unwrap_or(0)
        } else {
            let heads = info.head_count.filter(|heads| *heads > 0)?;
            let kv_heads = info.head_count_kv.unwrap_or(heads);
            let fallback = || Some(info.embedding_length? / heads);
            let key = info.key_length.or_else(fallback)?;
            let value = info.value_length.or_else(fallback)?;
            kv_heads * (key + value)
        };
        let full_layers = match info.full_attention_interval.filter(|every| *every > 1) {
            Some(every) => layers.div_ceil(every),
            None => layers,
        };
        Some(Self {
            full_layers,
            windowed_layers: 0,
            window: 0,
            values_per_token_per_layer,
            max_positions: info.context_length,
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

/// Context to serve a GGUF model with. `llama-server` preallocates the whole KV cache, so the
/// requested window (else the model's native one) is capped at the largest context that keeps
/// weights plus cache within the comfortable memory budget, rounded down to 256 tokens.
///
/// Returns `Ok(None)` when the header or weight size cannot be read, or when there is no window
/// to serve. Returns `Ok` with the native (or requested) window when physical memory is unknown.
/// With known memory, returns an error instead of over-allocating when the budget cannot hold
/// the minimum serving context of 256 tokens (budget exhausted by weights and overhead, or too
/// little headroom left for 256 tokens of KV cache).
pub fn gguf_serving_context(
    path: &Path,
    requested: Option<u64>,
    kv_bits: Option<u8>,
) -> anyhow::Result<Option<u64>> {
    static MEMORY: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    let Some(info) = crate::llm::local_format::gguf_info(path) else {
        return Ok(None);
    };
    let Some(arch) = Architecture::from_gguf(&info) else {
        return Ok(None);
    };
    let Some(weights) = crate::llm::local_format::gguf_weight_bytes(path) else {
        return Ok(None);
    };
    let memory = *MEMORY.get_or_init(physical_memory_bytes);
    serving_context(weights, &arch, requested, kv_bits, memory)
}

/// Smallest context llama-server is started with; a known budget below it is refused.
const MIN_SERVING_CONTEXT: u64 = 256;

fn serving_context(
    weights: u64,
    arch: &Architecture,
    requested: Option<u64>,
    kv_bits: Option<u8>,
    memory: Option<u64>,
) -> anyhow::Result<Option<u64>> {
    let Some(wanted) = requested
        .filter(|tokens| *tokens > 0)
        .or(arch.max_positions)
        .map(|tokens| effective_context(tokens, arch))
    else {
        return Ok(None);
    };
    let Some(memory) = memory.filter(|m| *m > 0) else {
        // Memory unknown: no budget to enforce, so serve the wanted window as before.
        return Ok(Some((wanted / 256 * 256).max(MIN_SERVING_CONTEXT)));
    };
    let affordable = max_context(weights, arch, kv_bits, Some(memory));
    if affordable < MIN_SERVING_CONTEXT {
        let budget = (memory as f64 * COMFORTABLE_FRACTION) as u64;
        anyhow::bail!(
            "model does not fit in memory: weights {weights} bytes plus runtime overhead leave room for only {affordable} tokens of KV cache within the {budget}-byte comfortable budget (of {memory} bytes physical); llama-server needs at least {MIN_SERVING_CONTEXT}. Choose a smaller or more heavily quantized model"
        );
    }
    // `affordable >= 256`, so rounding up a sub-256 request to 256 stays inside the budget.
    let context = wanted.min(affordable);
    Ok(Some((context / 256 * 256).max(MIN_SERVING_CONTEXT)))
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
    fn gguf_layout_counts_kv_heads_and_only_attention_layers_of_hybrids() {
        let info = crate::llm::local_format::GgufInfo {
            block_count: Some(48),
            head_count: Some(32),
            head_count_kv: Some(4),
            key_length: Some(256),
            value_length: Some(256),
            context_length: Some(262_144),
            ..Default::default()
        };
        let dense = Architecture::from_gguf(&info).unwrap();
        assert_eq!(dense.full_layers, 48);
        assert_eq!(dense.values_per_token_per_layer, 4 * 512);
        assert_eq!(dense.max_positions, Some(262_144));
        let hybrid = Architecture::from_gguf(&crate::llm::local_format::GgufInfo {
            full_attention_interval: Some(4),
            ..info.clone()
        })
        .unwrap();
        assert_eq!(hybrid.full_layers, 12);
        let derived = Architecture::from_gguf(&crate::llm::local_format::GgufInfo {
            key_length: None,
            value_length: None,
            embedding_length: Some(4096),
            ..info
        })
        .unwrap();
        assert_eq!(derived.values_per_token_per_layer, 4 * 2 * 128);
    }

    #[test]
    fn serving_context_caps_native_windows_to_memory_and_honors_requests() {
        let layout = Architecture {
            max_positions: Some(262_144),
            ..arch(32, 2048)
        };
        let weights = 8 * GIB;
        let memory = Some(32 * GIB);
        let fitted = serving_context(weights, &layout, None, None, memory)
            .unwrap()
            .unwrap();
        assert!(fitted < 262_144 && fitted % 256 == 0, "{fitted}");
        assert!(resident_bytes(weights, &layout, fitted, None) <= (32 * GIB) * 6 / 10);
        assert_eq!(
            serving_context(weights, &layout, Some(32_768), None, memory).unwrap(),
            Some(32_768)
        );
        // Metadata without a window or request yields no context at all.
        assert_eq!(
            serving_context(weights, &arch(32, 2048), None, None, memory).unwrap(),
            None
        );
    }

    /// Weights chosen so that exactly `headroom_tokens`-worth of fp16 KV cache is left in the
    /// comfortable budget for `arch(2, 256)` on 16 GiB (1024 bytes per token).
    fn weights_leaving_tokens(headroom_tokens: u64) -> u64 {
        let budget = (16 * GIB) as f64 * COMFORTABLE_FRACTION;
        let fixed = FIXED_OVERHEAD_BYTES as f64;
        ((budget - fixed - (headroom_tokens * 1024) as f64) * 20.0 / 21.0) as u64
    }

    #[test]
    fn serving_context_unknown_memory_serves_native_or_requested_window() {
        let layout = Architecture {
            max_positions: Some(262_144),
            ..arch(2, 256)
        };
        let huge = 100 * GIB;
        assert_eq!(
            serving_context(huge, &layout, None, None, None).unwrap(),
            Some(262_144)
        );
        assert_eq!(
            serving_context(huge, &layout, Some(8192), None, None).unwrap(),
            Some(8192)
        );
    }

    #[test]
    fn serving_context_refuses_when_weights_exhaust_the_budget() {
        let layout = Architecture {
            max_positions: Some(262_144),
            ..arch(2, 256)
        };
        let memory = Some(16 * GIB);
        let error = serving_context(10 * GIB, &layout, None, None, memory).unwrap_err();
        assert!(
            error.to_string().contains("does not fit in memory"),
            "{error}"
        );
        assert!(serving_context(10 * GIB, &layout, Some(256), None, memory).is_err());
    }

    #[test]
    fn serving_context_refuses_when_headroom_is_below_minimum_context() {
        let layout = Architecture {
            max_positions: Some(262_144),
            ..arch(2, 256)
        };
        let memory = Some(16 * GIB);
        let weights = weights_leaving_tokens(100);
        let affordable = max_context(weights, &layout, None, memory);
        assert!((1..256).contains(&affordable), "{affordable}");
        assert!(serving_context(weights, &layout, None, None, memory).is_err());
        assert!(serving_context(weights, &layout, Some(64), None, memory).is_err());
    }

    #[test]
    fn serving_context_rounds_down_to_256_within_budget() {
        let layout = Architecture {
            max_positions: Some(262_144),
            ..arch(2, 256)
        };
        let memory = Some(16 * GIB);
        let weights = weights_leaving_tokens(300);
        let affordable = max_context(weights, &layout, None, memory);
        assert!((256..512).contains(&affordable), "{affordable}");
        // Requests below the minimum round up to it, which stays within the 256+ budget.
        assert_eq!(
            serving_context(weights, &layout, Some(100), None, memory).unwrap(),
            Some(256)
        );
        let capped = serving_context(weights, &layout, None, None, memory)
            .unwrap()
            .unwrap();
        assert_eq!(capped, 256);
        assert!(
            resident_bytes(weights, &layout, capped, None) as f64
                <= 16.0 * GIB as f64 * COMFORTABLE_FRACTION
        );
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

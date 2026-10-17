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
    assert!(fitted < 262_144 && fitted.is_multiple_of(256), "{fitted}");
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

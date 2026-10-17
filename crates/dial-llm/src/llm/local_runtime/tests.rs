use super::*;

fn runtime(draft: Option<&str>) -> LocalRuntime {
    LocalRuntime {
        draft_model: draft.map(str::to_string),
        ..LocalRuntime::default()
    }
}

#[test]
fn default_runtime_adds_no_server_flags() {
    assert!(LocalRuntime::default().is_default());
    assert!(LocalRuntime::default().server_args().is_empty());
}

#[test]
fn builds_draft_and_kv_flags_in_order() {
    let value = LocalRuntime {
        draft_model: Some("mlx-community/draft-0.5b".into()),
        num_draft_tokens: Some(5),
        kv_bits: Some(4),
        kv_group_size: Some(32),
        quantized_kv_start: Some(2048),
    };
    value.validate().unwrap();
    assert_eq!(
        value.server_args(),
        [
            "--draft-model=mlx-community/draft-0.5b",
            "--num-draft-tokens=5",
            "--kv-bits=4",
            "--kv-group-size=32",
            "--quantized-kv-start=2048",
        ]
    );
}

#[test]
fn draft_without_count_uses_mlx_default() {
    let args = runtime(Some("/models/draft")).server_args();
    assert_eq!(
        args[1],
        format!("--num-draft-tokens={DEFAULT_NUM_DRAFT_TOKENS}")
    );
}

#[test]
fn rejects_invalid_combinations() {
    let count_without_draft = LocalRuntime {
        num_draft_tokens: Some(3),
        ..LocalRuntime::default()
    };
    assert!(
        count_without_draft
            .validate()
            .unwrap_err()
            .to_string()
            .contains("requires draft_model")
    );

    let group_without_bits = LocalRuntime {
        kv_group_size: Some(64),
        ..LocalRuntime::default()
    };
    assert!(
        group_without_bits
            .validate()
            .unwrap_err()
            .to_string()
            .contains("requires kv_bits")
    );

    let start_without_bits = LocalRuntime {
        quantized_kv_start: Some(5000),
        ..LocalRuntime::default()
    };
    assert!(
        start_without_bits
            .validate()
            .unwrap_err()
            .to_string()
            .contains("requires kv_bits")
    );
}

#[test]
fn rejects_out_of_range_values() {
    let bits = LocalRuntime {
        kv_bits: Some(5),
        ..LocalRuntime::default()
    };
    assert!(
        bits.validate()
            .unwrap_err()
            .to_string()
            .contains("kv_bits must be one of")
    );

    let group = LocalRuntime {
        kv_bits: Some(8),
        kv_group_size: Some(16),
        ..LocalRuntime::default()
    };
    assert!(
        group
            .validate()
            .unwrap_err()
            .to_string()
            .contains("kv_group_size must be one of")
    );

    let count = LocalRuntime {
        draft_model: Some("d".into()),
        num_draft_tokens: Some(0),
        ..LocalRuntime::default()
    };
    assert!(
        count
            .validate()
            .unwrap_err()
            .to_string()
            .contains("between 1 and 16")
    );
}

#[test]
fn auto_and_blank_mean_unset_in_toml() {
    let parsed: LocalRuntime =
        toml::from_str("draft_model = \"\"\nnum_draft_tokens = \"auto\"\nkv_bits = \"AUTO\"\n")
            .unwrap();
    assert!(parsed.is_default());
    let auto_draft: LocalRuntime = toml::from_str("draft_model = \"auto\"\n").unwrap();
    assert!(auto_draft.is_default());
    let text = toml::to_string(&LocalRuntime::default()).unwrap();
    assert!(
        !text.contains("kv_bits"),
        "unset fields must not serialize: {text}"
    );
}

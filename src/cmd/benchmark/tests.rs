#[test]
fn policy_overrides_are_isolated_recorded_and_hashed() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().to_owned()).unwrap();
    let base: Config =
        toml::from_str("[providers.local]\nbase_url = 'https://127.0.0.1:8443/v1'\n").unwrap();
    let mut configuration = requested(Path::new("model"));
    configuration.provider = "local".into();
    configuration.quantization_bits = None;
    let (_, before) = select_configuration(
        &layout,
        &base,
        &mut configuration,
        &MachineBenchmarkPolicy::default(),
    )
    .unwrap();
    configuration.policy = PolicyOverrides {
        strict_requirements: Some(true),
        review_enabled: Some(true),
        review_min_changed_files: Some(2),
        review_max_rounds: Some(2),
        review_sensitive_paths: Some(vec!["src/security".into()]),
    };
    let (selected, settings) = select_configuration(
        &layout,
        &base,
        &mut configuration,
        &MachineBenchmarkPolicy::default(),
    )
    .unwrap();
    assert_eq!(settings["policy_contract_version"], 1);
    assert_eq!(settings["verification"]["strict_requirements"], true);
    assert_eq!(settings["verification"]["review"]["enabled"], true);
    assert_eq!(settings["verification"]["review"]["min_changed_files"], 2);
    assert_ne!(settings["policy_sha256"], before["policy_sha256"]);
    let policy_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&shipped_policy(&selected)).unwrap())
    );
    assert_eq!(settings["policy_sha256"], policy_hash);
    assert_eq!(shipped_policy(&base).strict_requirements, Some(false));
    let frozen = clone_project(root.path()).unwrap();
    let contract = |config: &Config| {
        dial::benchmark::records::Contract::new(
        &BenchmarkSuite::parse_toml("version=1\nname='policy'\n[[cases]]\nid='one'\nprompt='test'\nchecks=[{kind='file_exists',path='ok'}]\n").unwrap(),
        frozen.path(), &serde_json::to_value(config).unwrap()).unwrap()
    };
    assert_ne!(
        contract(&base).configuration_sha256,
        contract(&selected).configuration_sha256
    );
    configuration.policy.review_max_rounds = Some(4);
    assert!(
        select_configuration(
            &layout,
            &base,
            &mut configuration,
            &MachineBenchmarkPolicy::default()
        )
        .is_err()
    );
}

use super::*;

fn requested(model: &Path) -> MachineConfiguration {
    MachineConfiguration {
        name: "candidate".into(),
        provider: "mlx".into(),
        model: model.to_string_lossy().into_owned(),
        context_window: 8192,
        quantization_bits: Some(4),
        adapter: None,
        assistance: Default::default(),
        draft_model: None,
        num_draft_tokens: None,
        kv_bits: None,
        kv_group_size: None,
        quantized_kv_start: None,
        efficiency: Default::default(),
        policy: Default::default(),
        integration: Default::default(),
    }
}

#[test]
fn configuration_preflight_checks_quantization_and_adapter_identity() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().to_owned()).unwrap();
    let model = root.path().join("model");
    fs::create_dir(&model).unwrap();
    fs::write(
        model.join("config.json"),
        r#"{"quantization":{"bits":4,"group_size":64}}"#,
    )
    .unwrap();
    let mut configuration = requested(&model);
    let (selected, settings) = select_configuration(
        &layout,
        &Config::default(),
        &mut configuration,
        &MachineBenchmarkPolicy::default(),
    )
    .unwrap();
    assert_eq!(selected.context_window, 8192);
    assert_eq!(settings["quantization_metadata"]["bits"], 4);
    configuration.quantization_bits = Some(8);
    assert!(
        select_configuration(
            &layout,
            &selected,
            &mut configuration,
            &MachineBenchmarkPolicy::default()
        )
        .is_err()
    );
    configuration.quantization_bits = Some(4);
    let adapter = root.path().join("adapter");
    fs::create_dir(&adapter).unwrap();
    fs::write(adapter.join("adapters.safetensors"), []).unwrap();
    fs::write(
        adapter.join("adapter_config.json"),
        r#"{"model":"/different/model"}"#,
    )
    .unwrap();
    configuration.adapter = Some(adapter.clone());
    assert!(
        select_configuration(
            &layout,
            &selected,
            &mut configuration,
            &MachineBenchmarkPolicy::default()
        )
        .is_err()
    );
    fs::write(
        adapter.join("adapter_config.json"),
        serde_json::to_vec(&json!({"model":model})).unwrap(),
    )
    .unwrap();
    let (_, settings) = select_configuration(
        &layout,
        &selected,
        &mut configuration,
        &MachineBenchmarkPolicy::default(),
    )
    .unwrap();
    assert!(settings["adapter_metadata"]["model"].is_string());
}

#[test]
fn remote_inference_needs_explicit_opt_in() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().to_owned()).unwrap();
    let base: Config = toml::from_str(
        r#"
            [providers.remote]
            base_url = "https://example.com/v1"
        "#,
    )
    .unwrap();
    let mut configuration = MachineConfiguration {
        name: "remote".into(),
        provider: "remote".into(),
        model: "operator-model".into(),
        context_window: 8192,
        quantization_bits: None,
        adapter: None,
        assistance: Default::default(),
        draft_model: None,
        num_draft_tokens: None,
        kv_bits: None,
        kv_group_size: None,
        quantized_kv_start: None,
        efficiency: Default::default(),
        policy: Default::default(),
        integration: Default::default(),
    };
    let policy = MachineBenchmarkPolicy::default();
    assert!(
        select_configuration(&layout, &base, &mut configuration, &policy)
            .unwrap_err()
            .to_string()
            .contains("allow_remote")
    );
    let opted_in = MachineBenchmarkPolicy {
        allow_remote: true,
        ..policy
    };
    let (selected, _) =
        select_configuration(&layout, &base, &mut configuration, &opted_in).unwrap();
    assert_eq!(selected.model, "operator-model");
}

#[test]
fn assist_variants_change_exactly_one_feature_from_dial() {
    let configurations = assist_configurations(&Config::default(), "openai", "model");
    let names: Vec<_> = configurations.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[..2], ["raw", "dial"]);
    assert_eq!(names.len(), 4 + ASSIST_VARIANTS.len());
    assert_eq!(configurations[1].assistance.lean_local, Some(LeanLocal::On));
    let dial =
        json!({"assistance": configurations[1].assistance, "policy": configurations[1].policy});
    for variant in &configurations[2..] {
        let value = json!({"assistance": variant.assistance, "policy": variant.policy});
        let changed = ["assistance", "policy"]
            .iter()
            .map(|section| {
                dial[section]
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(key, before)| value[section][key.as_str()] != **before)
                    .count()
            })
            .sum::<usize>();
        assert_eq!(
            changed, 1,
            "{} must change exactly one feature",
            variant.name
        );
    }
}

#[test]
fn dial_baseline_tracks_shipped_defaults_and_ignores_user_config() {
    let shipped = Config::default();
    let mut user = Config::default();
    user.assistance.compact_tools = true;
    user.assistance.repair_hints = true;
    user.assistance.lean_local = LeanLocal::Off;
    user.efficiency.history_token_cap = 8192;
    let configurations = assist_configurations(&user, "openai", "model");
    let dial = &configurations[1];
    // Shipped: compact_tools and repair_hints are off, lean_local is on.
    assert_eq!(dial.assistance.compact_tools, Some(false));
    assert_eq!(dial.assistance.repair_hints, Some(false));
    assert_eq!(dial.assistance.lean_local, Some(LeanLocal::On));
    assert_eq!(
        dial.assistance.initial_tool_limit,
        Some(shipped.assistance.initial_tool_limit)
    );
    assert_eq!(
        dial.assistance.reliability_gate,
        Some(shipped.reliability.enabled)
    );
    assert_eq!(
        dial.efficiency.history_token_cap,
        Some(shipped.efficiency.history_token_cap)
    );
    // Default-off features are enabled by variants, not reported as ablations of dial.
    let enable = configurations
        .iter()
        .find(|c| c.name == "enable-compact_tools")
        .unwrap();
    assert_eq!(enable.assistance.compact_tools, Some(true));
    assert_eq!(configurations[2].assistance.lean_local, Some(LeanLocal::On));
    assert!(
        configurations
            .iter()
            .all(|c| c.efficiency == dial.efficiency)
    );
}

#[test]
fn efficiency_overrides_stay_in_the_isolated_config_and_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let layout = Layout::with_user_dir(root.path(), home.path().to_owned()).unwrap();
    let base: Config = toml::from_str(
        r#"
            [providers.local]
            base_url = "https://127.0.0.1:8443/v1"
        "#,
    )
    .unwrap();
    let mut configuration = MachineConfiguration {
        name: "context".into(),
        provider: "local".into(),
        model: "operator-model".into(),
        context_window: 8192,
        quantization_bits: None,
        adapter: None,
        assistance: Default::default(),
        efficiency: EfficiencyOverrides {
            tool_preview_bytes: Some(2048),
            history_token_cap: Some(65_536),
            compaction_enabled: Some(false),
            keep_recent_tokens: Some(4096),
        },
        draft_model: None,
        num_draft_tokens: None,
        kv_bits: None,
        kv_group_size: None,
        quantized_kv_start: None,
        policy: Default::default(),
        integration: Default::default(),
    };
    let policy = MachineBenchmarkPolicy::default();
    let (selected, settings) =
        select_configuration(&layout, &base, &mut configuration, &policy).unwrap();
    assert_eq!(selected.efficiency.tool_preview_bytes, 2048);
    assert_eq!(selected.efficiency.history_token_cap, 65_536);
    assert!(!selected.compaction.enabled);
    assert_eq!(selected.compaction.keep_recent_tokens, 4096);
    assert_eq!(settings["efficiency"]["history_token_cap"], 65_536);
    assert_eq!(settings["efficiency"]["tool_preview_bytes"], 2048);
    // The operator's base config is never mutated by an experiment override.
    let defaults = Config::default();
    assert_eq!(
        base.efficiency.history_token_cap,
        defaults.efficiency.history_token_cap
    );
    assert_eq!(
        base.efficiency.tool_preview_bytes,
        defaults.efficiency.tool_preview_bytes
    );
    assert_eq!(base.compaction.enabled, defaults.compaction.enabled);
    // Out-of-bounds values fail before any inference.
    configuration.efficiency.history_token_cap = Some(1);
    assert!(select_configuration(&layout, &base, &mut configuration, &policy).is_err());
    configuration.efficiency = EfficiencyOverrides {
        tool_preview_bytes: Some(10),
        ..Default::default()
    };
    assert!(select_configuration(&layout, &base, &mut configuration, &policy).is_err());
}

#[test]
fn machine_case_contract_is_identical_across_strict_variants_and_ignores_ambient_requirements() {
    let mut base = Config::default();
    base.verification.requirements.insert(
        "ambient".into(),
        dial::config::types::VerificationRequirement {
            checks: vec!["missing".into()],
            visual: false,
        },
    );
    assert!(isolated_config(&base).verification.requirements.is_empty());
    let case = BenchmarkSuite::parse_toml("version=1\nname='contract'\n[[cases]]\nid='one'\nprompt='test'\nchecks=[{kind='command',command='printf checked'}]\n").unwrap().cases.remove(0);
    let baseline = isolated_config(&base);
    let mapped = case_contract(&baseline, &case).unwrap();
    assert_eq!(mapped.verification.commands.len(), 1);
    assert_eq!(mapped.verification.requirements.len(), 1);
    let mut strict = baseline.clone();
    strict.verification.strict_requirements = true;
    let strict = case_contract(&strict, &case).unwrap();
    assert_eq!(
        serde_json::to_value(&strict.verification.commands).unwrap(),
        serde_json::to_value(&mapped.verification.commands).unwrap()
    );
    let mut journey = case.clone();
    journey.turns.push(dial::benchmark::BenchmarkTurn {
        prompt: "correction".into(),
        compact_before: true,
    });
    let mut empty_strict = baseline;
    empty_strict.verification.strict_requirements = true;
    assert!(
        case_contract(&empty_strict, &journey)
            .unwrap_err()
            .is::<dial::benchmark::AgentUnavailable>()
    );
}

#[test]
fn review_experiment_pins_measured_model_at_both_isolation_points() {
    let mut config = Config {
        model: "measured".into(),
        ..Config::default()
    };
    config.verification.review.enabled = true;
    config.model_roles.insert(
        "coach".into(),
        dial::llm::model_config::ModelSelection::new("mlx", "ambient-coach"),
    );
    let selected = integration::isolate(&config, &Default::default()).unwrap();
    assert_eq!(selected.model_roles["coach"].model, "measured");
    let second = integration::isolate(&selected, &Default::default()).unwrap();
    assert_eq!(second.model_roles["coach"], selected.selection());
    assert_eq!(second.model_roles.len(), 1);
}

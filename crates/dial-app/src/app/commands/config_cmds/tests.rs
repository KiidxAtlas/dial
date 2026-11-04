use super::*;

#[test]
fn preset_names_follow_omp_rule() {
    assert!(valid_name("fast-1_x"));
    assert!(!valid_name("1abc"));
    assert!(!valid_name("a.b"));
    assert!(!valid_name(""));
}

#[test]
fn ssh_add_parses_flags_and_scope() {
    let (name, host, scope) =
        parse_ssh_add("box --host 10.0.0.2 --user dev --port 2222 --key ~/.ssh/id --scope project")
            .unwrap();
    assert_eq!(name, "box");
    assert_eq!(host.host, "10.0.0.2");
    assert_eq!(host.user.as_deref(), Some("dev"));
    assert_eq!(host.port, Some(2222));
    assert_eq!(host.key.as_deref(), Some("~/.ssh/id"));
    assert_eq!(scope, Scope::Project);
}

#[test]
fn ssh_add_rejects_missing_host_and_bad_port() {
    assert!(parse_ssh_add("box").is_err());
    assert!(parse_ssh_add("box --host h --port 0").is_err());
    assert!(parse_ssh_add("box --host h --port 70000").is_err());
    assert!(parse_ssh_add("box --host").is_err());
}

#[test]
fn ssh_remove_parses_scope() {
    assert_eq!(
        parse_ssh_remove("box").unwrap(),
        ("box".into(), Scope::Global)
    );
    assert_eq!(
        parse_ssh_remove("box --scope project").unwrap(),
        ("box".into(), Scope::Project)
    );
    assert!(parse_ssh_remove("box --scope nope").is_err());
}

fn written(changes: &[Change]) -> Vec<String> {
    changes
        .iter()
        .map(|change| match change {
            Change::Set { path, .. } | Change::Insert { path, .. } | Change::Remove { path } => {
                path.clone()
            }
        })
        .collect()
}

#[test]
fn slow_writes_only_slow_mode_and_leaves_fast_alone() {
    let mut config = Config {
        fast_mode: true,
        ..Default::default()
    };
    let plan = slow_plan(&config, "openai", "on").unwrap();
    assert_eq!(written(&plan.changes), vec!["slow_mode"]);
    config.slow_mode = true;
    let plan = slow_plan(&config, "openai", "off").unwrap();
    assert_eq!(written(&plan.changes), vec!["slow_mode"]);
}

#[test]
fn fast_writes_only_fast_state_and_leaves_slow_alone() {
    let config = Config {
        slow_mode: true,
        ..Default::default()
    };
    let plan = fast_plan(&config, "openai", "on").unwrap();
    assert_eq!(written(&plan.changes), vec!["fast_mode"]);
    let plan = fast_plan(&config, "openai", "off").unwrap();
    assert!(written(&plan.changes).is_empty());
}

#[test]
fn fast_off_clears_explicit_fast_tier_only() {
    let config = Config {
        service_tier: "ultrafast".into(),
        ..Default::default()
    };
    let plan = fast_plan(&config, "openai", "off").unwrap();
    assert_eq!(written(&plan.changes), vec!["service_tier"]);
}

#[test]
fn busy_fast_commands_plan_from_latest_saved_settings_and_can_return_to_base() {
    use crate::app::shell::notes::{settings_config, stage_settings};

    let (_home, _project, layout, _) = crate::app::tests::config();
    settings_store::save_config(
        &layout,
        Scope::Global,
        &[
            set_value("provider", "openai".into()),
            set_value("model", "gpt-4.1".into()),
        ],
    )
    .unwrap();
    let active = Config::load(&layout, None).unwrap().for_coding();
    let mut pending = None;
    for (args, expected) in [
        ("on", true),
        ("off", false),
        ("toggle", true),
        ("toggle", false),
    ] {
        let config = settings_config(&active, &pending);
        let plan = fast_plan(config, &config.selection().provider, args).unwrap();
        assert!(
            !plan.changes.is_empty(),
            "{args} must not be planned against stale state"
        );
        settings_store::save_config(&layout, Scope::Global, &plan.changes).unwrap();
        let fresh = Config::load(&layout, None).unwrap().for_coding();
        assert_eq!(fresh.fast_mode, expected);
        stage_settings(&active, &mut pending, fresh);
        assert_eq!(settings_config(&active, &pending).fast_mode, expected);
        assert_eq!(
            pending.is_some(),
            expected,
            "returning to base clears stale pending settings"
        );
        let status = fast_plan(settings_config(&active, &pending), "openai", "status").unwrap();
        assert_eq!(
            status.note,
            format!("Fast mode is {}.", if expected { "on" } else { "off" })
        );
    }
    assert!(
        !active.fast_mode,
        "the running agent retains its immutable settings"
    );
}

#[test]
fn busy_settings_lookup_sees_new_entries_without_changing_the_running_config() {
    use crate::app::shell::notes::{settings_config, stage_settings};

    let (_home, _project, layout, active) = crate::app::tests::config();
    let (name, host, scope) = parse_ssh_add("newbox --host example.test").unwrap();
    settings_store::save_config(
        &layout,
        scope,
        &[Change::Set {
            path: format!("ssh_hosts.{name}"),
            value: toml::Value::try_from(&host).unwrap(),
        }],
    )
    .unwrap();
    let mut pending = None;
    stage_settings(&active, &mut pending, Config::load(&layout, None).unwrap());
    assert!(
        settings_config(&active, &pending)
            .ssh_hosts
            .contains_key(&name)
    );
    assert!(!active.ssh_hosts.contains_key(&name));
    settings_store::save_config(
        &layout,
        scope,
        &[Change::Remove {
            path: format!("ssh_hosts.{name}"),
        }],
    )
    .unwrap();
    stage_settings(&active, &mut pending, Config::load(&layout, None).unwrap());
    assert!(
        !settings_config(&active, &pending)
            .ssh_hosts
            .contains_key(&name)
    );
    assert!(pending.is_none());
}

#[test]
fn set_values_use_schema_ranges_types_and_unknown_key_suggestions() {
    for (key, raw, valid) in [
        ("ui.tips", "true", true),
        ("ui.tips", "not-a-bool", false),
        ("harness.max_steps", "-1", false),
        ("harness.max_steps", "64", true),
    ] {
        assert_eq!(setting_value(key, raw).is_ok(), valid, "{key} {raw}");
    }
    let error = setting_value("ui.tip", "false").unwrap_err().to_string();
    assert!(error.contains("Did you mean \"ui.tips\""), "{error}");
}

#[test]
fn working_setup_capture_roundtrips_without_models_secrets_or_paths() {
    let mut config = Config {
        fast_mode: true,
        ..Default::default()
    };
    config.model_roles.insert(
        "coach".into(),
        crate::llm::model_config::ModelSelection::new("mlx", "/private/model"),
    );
    config.laya.checkpoint = Some("/private/checkpoint".into());
    let preset = capture_setup(&config);
    let encoded = toml::to_string(&preset).unwrap();
    assert!(!encoded.contains("/private"));
    assert!(preset.roles.is_empty());
    assert!(preset.vision.is_none());
    assert!(preset.laya_checkpoint.is_none());
    let loaded: ModelPreset = toml::from_str(&encoded).unwrap();
    let changes = preset_changes(&config, &loaded).unwrap();
    let (_home, _project, layout, _) = crate::app::tests::config();
    let before = settings_store::load_layer(&layout, Scope::Global).unwrap();
    let future =
        settings_store::preview_config(&layout, Scope::Global, &changes, &toml::Table::new())
            .unwrap();
    assert!(future.fast_mode);
    assert_eq!(
        before,
        settings_store::load_layer(&layout, Scope::Global).unwrap(),
        "preview must not publish"
    );
    settings_store::save_config(&layout, Scope::Global, &changes).unwrap();
    let saved = Config::load(&layout, None).unwrap();
    assert_eq!(capture_setup(&saved).setup, loaded.setup);
    assert!(!written(&changes).contains(&"model_roles".into()));
}

#[test]
fn setup_rejects_unsafe_unknown_nested_and_colliding_keys() {
    for text in [
        "[setup]\nendpoint = true",
        "[setup]\n\"endpoint.headers\" = true",
        "[setup]\n\"memory.path\" = true",
        "[setup.memory]\nenabled = true",
        "[setup]\nfast_mode = '/private/path'",
        "[setup]\nfast_mode = true\nfast_mode = false",
        "[setup]\n\"memory.enabled\" = true\nmemory = true",
    ] {
        assert!(toml::from_str::<ModelPreset>(text).is_err(), "{text}");
    }
    let mut preset = ModelPreset::default();
    preset.setup.insert("endpoint.headers".into(), true.into());
    assert!(preset_changes(&Config::default(), &preset).is_err());
}

#[test]
fn legacy_model_presets_keep_the_same_change_plan_and_redact_review_auth() {
    let preset: ModelPreset =
        toml::from_str("laya_checkpoint = '/local/checkpoint'\n[roles]").unwrap();
    let changes = preset_changes(&Config::default(), &preset).unwrap();
    assert_eq!(written(&changes), ["model_roles", "laya.checkpoint"]);
    let empty: ModelPreset = toml::from_str("").unwrap();
    assert_eq!(
        written(&preset_changes(&Config::default(), &empty).unwrap()),
        ["model_roles"]
    );
    let review = changes_preview(&[Change::Set {
        path: "model_roles".into(),
        value: toml::toml! { [coach] model = "reviewed-model" headers = { Authorization = "secret-value" } }.into(),
    }, Change::Remove { path: "roles.task".into() }]);
    assert!(review.contains("reviewed-model"));
    assert!(review.contains("roles.task"));
    assert!(review.contains("[redacted]"));
    assert!(!review.contains("secret-value"));
}

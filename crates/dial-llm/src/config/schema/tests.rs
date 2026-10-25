use super::*;
use crate::{config::types::Config, llm::model_config::UiConfig};
use std::collections::HashSet;

fn default_table() -> Table {
    Table::try_from(Config::default()).expect("default config serializes")
}
#[test]
fn suggested_models_remain_open_and_output_bounds_follow_the_selected_profile() {
    check_value(
        "codebase.architecture_model",
        &Value::String("operator/custom-model".into()),
    )
    .unwrap();
    let mut config = Config::default();
    let selection = config.selection();
    config.model_profile = Some("fixture".into());
    config.model_profiles.insert(
        "fixture".into(),
        crate::llm::profiles::ModelProfile {
            provider: selection.provider,
            model: selection.model,
            max_tokens: Some(16384),
            ..Default::default()
        },
    );
    let values = Table::try_from(&config).unwrap();
    assert!(check_value("harness.max_tokens", &Value::Integer(12000)).is_err());
    check_value_for(&values, "harness.max_tokens", &Value::Integer(12000)).unwrap();
    assert!(check_value_for(&values, "harness.max_tokens", &Value::Integer(16385)).is_err());
}

#[test]
fn choice_settings_are_open_suggestions_not_closed_enums() {
    let def = registry()
        .iter()
        .find(|def| def.path == "memory.semantic.embedding_model")
        .expect("embedding model is a setting");
    assert!(matches!(def.kind, Kind::Choice(_)));
    assert_eq!(def.default_value(), Some(Value::String(String::new())));
    let constraints = def.constraints();
    assert!(constraints.get("enum").is_none());
    assert!(
        constraints["suggestions"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    for value in ["", "operator/custom-model:latest"] {
        check_value(
            "memory.semantic.embedding_model",
            &Value::String(value.into()),
        )
        .unwrap();
    }
    assert!(check_value("memory.semantic.embedding_model", &Value::Integer(1)).is_err());
}

#[test]
fn every_path_is_a_real_key_with_the_real_default() {
    let table = default_table();
    let ui = Table::try_from(UiConfig::default()).unwrap();
    for def in registry() {
        let default = def
            .default_value()
            .unwrap_or_else(|| panic!("{} has no declared default", def.path));
        let Some(actual) = get(&table, def.path) else {
            // `None` fields are left out of the serialized default. A blank or "auto" literal is exactly that unset state.
            let unset =
                default == Value::String(String::new()) || default == Value::String("auto".into());
            assert!(
                (def.path.starts_with("local_runtime.") || def.path.starts_with("lsp.")) && unset,
                "{} is not a key of the default config",
                def.path
            );
            continue;
        };
        assert_eq!(
            actual, &default,
            "{} default differs from the code's default",
            def.path
        );
        if let Some(key) = def.path.strip_prefix("ui.") {
            assert!(ui.contains_key(key), "{key} missing from UiConfig");
        }
    }
}

#[test]
fn no_duplicates_and_every_tab_and_section_has_entries() {
    let mut seen = HashSet::new();
    for def in registry() {
        assert!(seen.insert(def.path), "duplicate {}", def.path);
        assert!(!def.label.is_empty() && !def.description.is_empty());
    }
    let tabs = tabs();
    assert!(tabs.len() >= 5);
    for (tab, sections) in &tabs {
        assert!(!sections.is_empty());
        for section in sections {
            assert!(
                registry()
                    .iter()
                    .any(|d| d.tab == *tab && d.section == *section),
                "{tab}/{section} is empty"
            );
        }
    }
    assert_eq!(
        tabs.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        [
            "Appearance",
            "Interaction",
            "Model",
            "Memory",
            "Improvement",
            "Tools",
            "Experimental",
            "Voice"
        ]
    );
}

/// Sibling settings that `Config::validate` reads while `path` takes `value`. Each dependent is
/// set to a value that satisfies the rule for every value in the key's own range, so the key is
/// always validated against the real validator and no key is exempt.
fn cross_field_dependents(path: &str, value: &Value) -> Vec<(&'static str, Value)> {
    match path {
        "memory.semantic.embedding" if *value == Value::String("on".into()) => vec![
            (
                "memory.semantic.embedding_base_url",
                Value::String("http://127.0.0.1:8888/v1".into()),
            ),
            (
                "memory.semantic.embedding_model",
                Value::String("fixture-embedding".into()),
            ),
        ],
        // The hindsight backend requires an endpoint (config.rs validate).
        "memory.backend" if *value == Value::String("hindsight".into()) => vec![(
            "memory.hindsight.endpoint",
            Value::String("http://127.0.0.1:8888".into()),
        )],
        // The timeout must not exceed either lease; the heartbeat sits strictly inside the
        // consolidation lease (1 <= heartbeat < lease).
        "memory.local.timeout_secs" => vec![
            ("memory.local.stage1_lease_secs", Value::Integer(86400)),
            (
                "memory.local.consolidation_lease_secs",
                Value::Integer(86400),
            ),
            (
                "memory.local.consolidation_heartbeat_secs",
                Value::Integer(30),
            ),
        ],
        "memory.local.stage1_lease_secs" => {
            vec![("memory.local.timeout_secs", Value::Integer(1))]
        }
        "memory.local.consolidation_lease_secs" => vec![
            ("memory.local.timeout_secs", Value::Integer(1)),
            (
                "memory.local.consolidation_heartbeat_secs",
                Value::Integer(1),
            ),
        ],
        "memory.local.consolidation_heartbeat_secs" => vec![
            ("memory.local.timeout_secs", Value::Integer(1)),
            (
                "memory.local.consolidation_lease_secs",
                Value::Integer(86400),
            ),
        ],
        // The decision lease must cover its model deadline and stay within a day.
        "memory.sharpshooter.lease_secs" => {
            vec![("memory.sharpshooter.timeout_secs", Value::Integer(1))]
        }
        "memory.sharpshooter.timeout_secs" => {
            vec![("memory.sharpshooter.lease_secs", Value::Integer(86400))]
        }
        // The net-win threshold may not exceed the primary case count; 1000 is its maximum.
        "improve.min_net_wins" => vec![("improve.gate_primary_cases", Value::Integer(1000))],
        _ => Vec::new(),
    }
}

#[test]
fn every_enum_value_deserializes_and_ranges_are_accepted_by_the_config() {
    let mut base = default_table();
    // Memory backends that call a model require the memory role (config validate).
    set(
        &mut base,
        "model_roles.memory.provider",
        Value::String("mlx".into()),
    );
    set(
        &mut base,
        "model_roles.memory.model",
        Value::String("m".into()),
    );
    for def in registry() {
        let mut values: Vec<Value> = Vec::new();
        match def.kind {
            Kind::Enum(options) => {
                assert!(
                    options
                        .iter()
                        .any(|(v, _)| Some(Value::String((*v).into())) == def.default_value()),
                    "{} default is not an option",
                    def.path
                );
                values.extend(options.iter().map(|(v, _)| Value::String((*v).into())));
            }
            Kind::Choice(options) => {
                // Suggestions are not a closed set: the default may be blank and any text is valid.
                values.extend(options.iter().map(|(v, _)| Value::String((*v).into())));
                values.extend([
                    Value::String(String::new()),
                    Value::String("operator/custom-model".into()),
                ]);
            }
            Kind::AutoBool => {
                values.extend([
                    Value::Boolean(true),
                    Value::Boolean(false),
                    Value::String("auto".into()),
                ]);
            }
            Kind::Int { min, max, step } | Kind::AutoInt { min, max, step } => {
                assert!(
                    min < max && step > 0 && step <= max - min,
                    "{} bad int bounds",
                    def.path
                );
                values.extend([Value::Integer(min), Value::Integer(max)]);
                if matches!(def.kind, Kind::AutoInt { .. }) {
                    values.push(Value::String("auto".into()));
                }
            }
            Kind::Float { min, max, step } => {
                assert!(min < max && step > 0.0, "{} bad float bounds", def.path);
                values.extend([Value::Float(min), Value::Float(max)]);
            }
            _ => {}
        }
        for value in values {
            let mut table = base.clone();
            set(&mut table, def.path, value.clone());
            for (dependent, dependent_value) in cross_field_dependents(def.path, &value) {
                set(&mut table, dependent, dependent_value);
            }
            let config: Config = Value::Table(table)
                .try_into()
                .unwrap_or_else(|e| panic!("{} = {value} rejected: {e}", def.path));
            config
                .validate()
                .unwrap_or_else(|e| panic!("{} = {value}: {e}", def.path));
        }
    }
}

#[test]
fn service_tier_options_match_the_config_tiers() {
    let options = registry()
        .iter()
        .find(|setting| setting.path == "service_tier")
        .and_then(|setting| match setting.kind {
            Kind::Enum(options) => Some(options),
            _ => None,
        })
        .expect("service_tier is an enum setting");
    let values: Vec<&str> = options.iter().map(|(value, _)| *value).collect();
    assert_eq!(values, crate::config::SERVICE_TIERS);
}

#[test]
fn numeric_defaults_lie_inside_their_ranges() {
    for def in registry() {
        match (def.kind, def.default_value()) {
            (Kind::Int { min, max, .. }, Some(Value::Integer(n))) => {
                assert!((min..=max).contains(&n), "{}", def.path)
            }
            (Kind::Float { min, max, .. }, Some(Value::Float(n))) => {
                assert!(n >= min && n <= max, "{}", def.path)
            }
            (Kind::Int { .. } | Kind::Float { .. }, other) => {
                panic!("{} default {other:?} has the wrong type", def.path)
            }
            _ => {}
        }
    }
}
#[test]
fn experimental_options_are_registered_in_their_tab_and_default_off() {
    let paths = [
        "experimental.work_packet_contracts",
        "experimental.verification_receipts",
        "experimental.context_overlap_dedup",
        "experimental.profile_matching",
        "experimental.tool_speculation",
        "experimental.turn_optimizer",
    ];
    for path in paths {
        let setting = registry()
            .iter()
            .find(|setting| setting.path == path)
            .unwrap();
        assert_eq!(setting.tab, "Experimental");
        assert_eq!(setting.kind, Kind::Bool);
        assert_eq!(setting.default_value(), Some(Value::Boolean(false)));
    }
    let config: Config = toml::from_str("[experimental]\ncontext_overlap_dedup = true").unwrap();
    assert!(config.experimental.context_overlap_dedup);
    assert!(!config.experimental.verification_receipts);
}

#[test]
fn dotted_get_set_unset() {
    let mut t = Table::new();
    set(&mut t, "a.b.c", Value::Integer(1));
    assert_eq!(get(&t, "a.b.c"), Some(&Value::Integer(1)));
    assert_eq!(get(&t, "a.b.d"), None);
    assert_eq!(get(&t, "a.b.c.d"), None);
    set(&mut t, "a.b", Value::Boolean(true));
    assert_eq!(get(&t, "a.b"), Some(&Value::Boolean(true)));
    assert!(unset(&mut t, "a.b"));
    assert!(t.is_empty(), "empty parents are pruned");
    assert!(!unset(&mut t, "a.b"));
}

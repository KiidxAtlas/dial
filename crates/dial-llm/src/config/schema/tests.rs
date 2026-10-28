use super::*;
use crate::{config::types::Config, llm::model_config::UiConfig};
use std::collections::HashSet;

fn default_table() -> Table {
    Table::try_from(Config::default()).expect("default config serializes")
}

#[test]
fn prefer_subagents_defaults_off_for_new_and_existing_configs() {
    assert!(!Config::default().task.prefer_subagents);
    for source in [
        "",
        "[task]\n",
        "[task]\nmax_concurrency = 4\npersist = false\n",
    ] {
        let config: Config = toml::from_str(source).unwrap();
        assert!(!config.task.prefer_subagents, "{source}");
    }
    let existing: Config =
        toml::from_str("[task]\nmax_concurrency = 4\npersist = false\n").unwrap();
    assert_eq!(existing.task.max_concurrency, 4);
    assert!(!existing.task.persist);
}

#[test]
fn prefer_subagents_is_a_boolean_setting_and_roundtrips_both_values() {
    let path = "task.prefer_subagents";
    let setting = registry()
        .iter()
        .find(|setting| setting.path == path)
        .expect("subagent preference is exposed in settings");
    assert_eq!(setting.kind, Kind::Bool);
    assert_eq!(setting.default_value(), Some(Value::Boolean(false)));
    assert_eq!(
        setting.constraints(),
        serde_json::json!({"type": "boolean"})
    );
    for enabled in [false, true] {
        let value = Value::Boolean(enabled);
        check_value(path, &value).unwrap();
        let mut settings = default_table();
        set(&mut settings, path, value.clone());
        let config: Config = Value::Table(settings).try_into().unwrap();
        config.validate().unwrap();
        assert_eq!(config.task.prefer_subagents, enabled);
        let saved = toml::to_string(&config).unwrap();
        let loaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(loaded.task.prefer_subagents, enabled);
        assert_eq!(get(&Table::try_from(loaded).unwrap(), path), Some(&value));
    }
    for value in [
        Value::String("true".into()),
        Value::String("auto".into()),
        Value::Integer(1),
    ] {
        assert!(check_value(path, &value).is_err(), "accepted {value}");
        let mut settings = default_table();
        set(&mut settings, path, value);
        assert!(Value::Table(settings).try_into::<Config>().is_err());
    }
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
            // Optional values have no serialized default. Text uses an empty editor seed;
            // structured optional values use an empty table seed, not a persisted override.
            let optional = matches!(
                def.path,
                "provider"
                    | "endpoint"
                    | "model_profile"
                    | "gate"
                    | "snowflake_account"
                    | "cloudflare_account_id"
                    | "cloudflare_gateway_id"
                    | "image.api_key_env"
            );
            let unset = default == Value::String(String::new())
                || default == Value::String("auto".into())
                || (matches!(def.kind, Kind::Toml) && default == Value::Table(Table::new()));
            assert!(
                (optional
                    || def.path.starts_with("local_runtime.")
                    || def.path.starts_with("lsp."))
                    && unset,
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
fn experimental_options_are_registered_in_their_tab_with_their_defaults() {
    let paths = [
        "experimental.work_packet_contracts",
        "experimental.verification_receipts",
        "experimental.context_overlap_dedup",
        "experimental.profile_matching",
        "experimental.tool_speculation",
        "experimental.turn_optimizer",
        "experimental.streaming_dispatch",
        "experimental.check_predictions",
    ];
    let setting = |path: &str| {
        registry()
            .iter()
            .find(|setting| setting.path == path)
            .unwrap()
    };
    for path in paths {
        let setting = setting(path);
        assert_eq!(setting.tab, "Experimental");
        assert_eq!(setting.kind, Kind::Bool);
        assert_eq!(setting.default_value(), Some(Value::Boolean(false)));
    }
    for path in [
        "experimental.edit_bisection_runs",
        "experimental.outcome_gradient_rounds",
    ] {
        assert_eq!(setting(path).tab, "Experimental");
        assert_eq!(setting(path).default_value(), Some(Value::Integer(0)));
    }
    // Check-before-change aids ship enabled; the rest stay opt-in.
    let witness = setting("experimental.fault_witness");
    assert_eq!(witness.tab, "Experimental");
    assert_eq!(witness.kind, Kind::Bool);
    assert_eq!(witness.default_value(), Some(Value::Boolean(true)));
    let repro = setting("experimental.repro_first");
    assert_eq!(repro.tab, "Experimental");
    assert_eq!(repro.default_value(), Some(Value::String("advise".into())));
    let unset: Config = toml::from_str("").unwrap();
    assert!(unset.experimental.fault_witness);
    assert_eq!(
        unset.experimental.repro_first,
        crate::config::types::ReproFirst::Advise
    );
    let opted_out: Config =
        toml::from_str("[experimental]\nfault_witness = false\nrepro_first = \"off\"").unwrap();
    assert!(!opted_out.experimental.fault_witness);
    assert_eq!(
        opted_out.experimental.repro_first,
        crate::config::types::ReproFirst::Off
    );
    let config: Config = toml::from_str(
        "[experimental]\ncontext_overlap_dedup = true\nrepro_first = \"gate\"\nedit_bisection_runs = 4",
    )
    .unwrap();
    assert!(config.experimental.context_overlap_dedup);
    assert!(!config.experimental.verification_receipts);
    assert_eq!(
        config.experimental.repro_first,
        crate::config::types::ReproFirst::Gate
    );
    assert_eq!(config.experimental.edit_bisection_runs, 4);
    let mut out_of_range = Config::default();
    out_of_range.experimental.outcome_gradient_rounds = 11;
    assert!(out_of_range.validate().is_err());
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

#[test]
fn every_serialized_setting_has_a_management_route() {
    fn visit(value: &serde_json::Value, path: &str, missing: &mut Vec<String>) {
        if registry().iter().any(|def| def.path == path) {
            return;
        }
        if let serde_json::Value::Object(table) = value
            && !table.is_empty()
        {
            for (key, value) in table {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                visit(value, &child, missing);
            }
            return;
        }
        missing.push(path.to_owned());
    }
    let mut missing = Vec::new();
    // JSON retains optional null fields that TOML necessarily omits.
    visit(
        &serde_json::to_value(Config::default()).unwrap(),
        "",
        &mut missing,
    );
    assert!(
        missing.is_empty(),
        "Missing settings UI routes:\n{}",
        missing.join("\n")
    );
}

// Serde's field list includes optional fields omitted by default serialization. Skipped runtime
// fields are absent. This guards new top-level Config and UiConfig settings without a hand-kept
// duplicate list. Parent TOML routes manage every optional/dynamic descendant of their table.
#[test]
fn optional_and_ui_fields_have_routes_too() {
    use serde::Deserialize;
    struct Fields<'a>(&'a mut Vec<&'static str>);
    impl<'de> serde::Deserializer<'de> for Fields<'_> {
        type Error = serde::de::value::Error;
        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            _: V,
        ) -> Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom("field inventory only"))
        }
        fn deserialize_struct<V: serde::de::Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            self.0.extend(fields);
            Err(serde::de::Error::custom("field inventory only"))
        }
        serde::forward_to_deserialize_any! { bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map enum identifier ignored_any }
    }
    let mut fields = Vec::new();
    let _ = Config::deserialize(Fields(&mut fields));
    assert!(!fields.is_empty());
    for field in fields {
        assert!(
            registry()
                .iter()
                .any(|def| def.path == field || def.path.starts_with(&format!("{field}."))),
            "Config.{field} lacks a management route"
        );
    }
    let mut fields = Vec::new();
    let _ = UiConfig::deserialize(Fields(&mut fields));
    for field in fields {
        // Retired compatibility input; never read or serialized (model_config::UiConfig).
        if field == "hyperlinks" {
            continue;
        }
        assert!(
            registry()
                .iter()
                .any(|def| def.path == format!("ui.{field}")),
            "UiConfig.{field} lacks a management route"
        );
    }
}

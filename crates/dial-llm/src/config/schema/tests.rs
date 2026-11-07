use super::*;

#[test]
fn execution_gap_experiments_have_explicit_defaults_and_validate_bounds() {
    let defaults = crate::config::types::Config::default();
    assert!(defaults.eval.portable_helpers && defaults.eval.auto_offer && defaults.eval.parallel_batches);
    assert!(defaults.efficiency.preserve_task_inputs && defaults.verification.auto_registered_checks);
    assert!(!defaults.experimental.adaptive_output_effort);
    assert_eq!(defaults.compaction.argument_projection_tokens, 0);
    let configured: crate::config::types::Config = toml::from_str("[eval]\nportable_helpers=false\nauto_offer=false\nparallel_batches=false\n[efficiency]\npreserve_task_inputs=false\nevidence_stall_rounds=0\nsmall_case_guard=false\n[verification]\nauto_registered_checks=false\n").unwrap();
    assert!(!configured.eval.portable_helpers && !configured.eval.auto_offer && !configured.eval.parallel_batches);
    assert!(!configured.verification.auto_registered_checks);
    let mut invalid = defaults;
    invalid.efficiency.evidence_stall_rounds = 257;
    assert!(invalid.validate().is_err());
    invalid.efficiency.evidence_stall_rounds = 0;
    invalid.compaction.argument_projection_tokens = 4096;
    assert!(invalid.validate().is_err());
}
use crate::{config::types::Config, llm::model_config::UiConfig};
use std::collections::HashSet;

fn default_table() -> Table {
    Table::try_from(Config::default()).expect("default config serializes")
}

#[test]
fn dispatch_defaults_are_shared_by_deserialization_and_settings_with_an_opt_out() {
    let config: Config = toml::from_str("").unwrap();
    assert!(config.experimental.streaming_dispatch);
    assert!(config.experimental.dependency_dispatch);
    assert!(!config.experimental.batch_approvals);
    for path in [
        "experimental.streaming_dispatch",
        "experimental.dependency_dispatch",
    ] {
        let setting = registry().iter().find(|s| s.path == path).unwrap();
        assert_eq!(setting.default_value(), Some(Value::Boolean(true)));
        let mut settings = default_table();
        set(&mut settings, path, Value::Boolean(false));
        let config: Config = Value::Table(settings).try_into().unwrap();
        assert!(!if path.ends_with("streaming_dispatch") {
            config.experimental.streaming_dispatch
        } else {
            config.experimental.dependency_dispatch
        });
    }
}

#[test]
fn busy_send_is_explicit_typed_and_persisted_with_a_safe_default() {
    use crate::llm::model_config::BusySend;

    for source in ["", "[ui]\n", "[ui]\nhistory_limit = 20"] {
        let config: Config = toml::from_str(source).unwrap();
        assert_eq!(config.ui.busy_send, BusySend::Choose);
    }
    let path = "ui.busy_send";
    let setting = registry().iter().find(|s| s.path == path).unwrap();
    assert_eq!(
        setting.default_value(),
        Some(Value::String("choose".into()))
    );
    for (stored, preference) in [
        ("choose", BusySend::Choose),
        ("redirect", BusySend::Redirect),
        ("after_turn", BusySend::AfterTurn),
        ("independent", BusySend::Independent),
        ("auto", BusySend::Auto),
    ] {
        let value = Value::String(stored.into());
        check_value(path, &value).unwrap();
        let mut settings = default_table();
        set(&mut settings, path, value);
        let config: Config = Value::Table(settings).try_into().unwrap();
        assert_eq!(config.ui.busy_send, preference);
        let saved = toml::to_string(&config).unwrap();
        let loaded: Config = toml::from_str(&saved).unwrap();
        assert_eq!(loaded.ui.busy_send, preference);
    }
    for value in [
        Value::Boolean(true),
        Value::String("learned".into()),
        Value::String("after-turn".into()),
    ] {
        assert!(check_value(path, &value).is_err());
        let mut settings = default_table();
        set(&mut settings, path, value);
        assert!(Value::Table(settings).try_into::<Config>().is_err());
    }
}

#[test]
fn video_analysis_is_opt_in_and_typed() {
    for source in [
        "",
        "[experimental]\n",
        "[experimental]\nvideo_analysis = false",
    ] {
        let config: Config = toml::from_str(source).unwrap();
        assert!(!config.experimental.video_analysis);
    }
    let config: Config = toml::from_str("[experimental]\nvideo_analysis = true").unwrap();
    assert!(config.experimental.video_analysis);
    assert!(toml::from_str::<Config>("[experimental]\nvideo_analysis = 1").is_err());
    let setting = registry()
        .iter()
        .find(|s| s.path == "experimental.video_analysis")
        .unwrap();
    assert_eq!(setting.kind, Kind::Bool);
    assert_eq!(setting.default_value(), Some(Value::Boolean(false)));
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
                    | "image.api"
                    | "image.api_key_env"
                    | "laya.checkpoint"
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
        "experimental.turn_drafter_diagrams",
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
    // Reproduce-first advice stays enabled; expensive counterfactual runs are opt-in.
    let witness = setting("experimental.fault_witness");
    assert_eq!(witness.tab, "Experimental");
    assert_eq!(witness.kind, Kind::Bool);
    assert_eq!(witness.default_value(), Some(Value::Boolean(false)));
    let streaming = setting("experimental.streaming_dispatch");
    assert_eq!(streaming.tab, "Experimental");
    assert_eq!(streaming.default_value(), Some(Value::Boolean(true)));
    // Dependency dispatch graduated to on-by-default after reliability trials; `false` opts out.
    let dependency = setting("experimental.dependency_dispatch");
    assert_eq!(dependency.tab, "Experimental");
    assert_eq!(dependency.kind, Kind::Bool);
    assert_eq!(dependency.default_value(), Some(Value::Boolean(true)));
    let repro = setting("experimental.repro_first");
    assert_eq!(repro.tab, "Experimental");
    assert_eq!(repro.default_value(), Some(Value::String("advise".into())));
    let unset: Config = toml::from_str("").unwrap();
    assert!(!unset.experimental.fault_witness);
    assert!(unset.experimental.dependency_dispatch);
    assert_eq!(
        unset.experimental.repro_first,
        crate::config::types::ReproFirst::Advise
    );
    let opted_out: Config = toml::from_str(
        "[experimental]\nfault_witness = false\nrepro_first = \"off\"\ndependency_dispatch = false",
    )
    .unwrap();
    assert!(!opted_out.experimental.fault_witness);
    assert!(!opted_out.experimental.dependency_dispatch);
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
    let opted_in: Config = toml::from_str("[experimental]\nfault_witness = true").unwrap();
    assert!(opted_in.experimental.fault_witness);
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

#[test]
fn value_sources_match_registered_setting_kinds() {
    let sourced: Vec<_> = registry()
        .iter()
        .filter_map(|def| def.source().map(|source| (def, source)))
        .collect();
    let paths: HashSet<_> = sourced.iter().map(|(def, _)| def.path).collect();
    for expected in [
        "python",
        "worker",
        "eval.node",
        "ida.python",
        "ida.idadir",
        "sandbox.extra_write",
        "lsp.rust-analyzer.program",
        "local_runtime.draft_model",
        "codebase.architecture_model",
        "memory.sharpshooter.model",
        "training.learn_from",
        "image.model",
        "model",
        "provider",
        "model_profile",
        "ui.status_left",
        "ui.status_right",
        "compaction.method_order",
        "integrations",
        "foreign_sources",
        "memory.hindsight.recall_types",
    ] {
        assert!(paths.contains(expected), "{expected} has no value source");
    }
    for (def, source) in sourced {
        let list = matches!(def.kind, Kind::StringList);
        let single = matches!(def.kind, Kind::Text | Kind::Choice(_));
        let ok = match source {
            Source::Values { .. } => list,
            Source::Path(PathKind::Directory) if def.path == "sandbox.extra_write" => list,
            Source::Model(_) if def.path == "training.learn_from" => list,
            _ => single,
        };
        assert!(
            ok,
            "{} source {source:?} does not fit {:?}",
            def.path, def.kind
        );
    }
}

#[test]
fn known_list_values_are_accepted_by_their_settings() {
    for (path, values) in [
        ("compaction.method_order", COMPACTION_METHODS),
        ("foreign_sources", FOREIGN_SOURCES),
        ("memory.hindsight.recall_types", RECALL_TYPES),
        (
            "integrations",
            crate::config::tools::integrations::INTEGRATION_TOOLS,
        ),
    ] {
        let value = Value::Array(values.iter().map(|v| Value::String((*v).into())).collect());
        let mut table = default_table();
        check_value_for(&table, path, &value).unwrap_or_else(|e| panic!("{path}: {e}"));
        set(&mut table, path, value);
        let config: Config = table.try_into().unwrap_or_else(|e| panic!("{path}: {e}"));
        config
            .validate()
            .unwrap_or_else(|e| panic!("{path}: {e:#}"));
    }
    let methods: Vec<crate::config::compaction::CompactionMethod> = COMPACTION_METHODS
        .iter()
        .map(|m| serde_json::from_value(serde_json::json!(m)).unwrap())
        .collect();
    assert_eq!(methods, Config::default().compaction.method_order);
}

#[test]
fn layout_lists_every_registered_section_exactly_once() {
    for def in REGISTRY.iter().chain(management::DECLARATIONS) {
        let sections = LAYOUT
            .iter()
            .find(|(tab, _)| *tab == def.tab)
            .unwrap_or_else(|| panic!("tab {} of {} is missing from LAYOUT", def.tab, def.path))
            .1;
        assert!(
            sections.contains(&def.section),
            "section {}/{} of {} is missing from LAYOUT",
            def.tab,
            def.section,
            def.path
        );
    }
    for (tab, sections) in LAYOUT {
        let unique: HashSet<_> = sections.iter().collect();
        assert_eq!(unique.len(), sections.len(), "{tab} repeats a section");
        for section in *sections {
            assert!(
                REGISTRY
                    .iter()
                    .chain(management::DECLARATIONS)
                    .any(|def| def.tab == *tab && def.section == *section),
                "LAYOUT lists {tab}/{section}, which holds no setting"
            );
        }
    }
}

#[test]
fn tabs_follow_the_layout_table_not_declaration_order() {
    let tabs = tabs();
    let names: Vec<_> = tabs.iter().map(|(tab, _)| *tab).collect();
    let expected = vec![
        "Common",
        "Appearance",
        "Keys",
        "Model",
        "Connections",
        "Safety",
        "Tools",
        "Memory",
        "Learning",
        "Experimental",
        "Advanced",
    ];
    assert_eq!(names, expected);
    let common = tabs.first().unwrap();
    assert_eq!(common.1, vec!["Common"]);
    for path in COMMON_SETTINGS {
        assert!(
            registry()
                .iter()
                .any(|def| def.path == *path && def.tab == "Common"),
            "{path}"
        );
    }
}

#[test]
fn presentation_groups_keep_canonical_contracts_and_original_locations() {
    for original in REGISTRY.iter().chain(management::DECLARATIONS) {
        let presented = registry()
            .iter()
            .find(|def| def.path == original.path)
            .unwrap();
        assert_eq!(
            presented.search_location(),
            (original.tab, original.section)
        );
        assert_eq!(presented.kind, original.kind, "{}", original.path);
        assert_eq!(
            presented.default_value(),
            original.default_value(),
            "{}",
            original.path
        );
        assert_eq!(presented.restart, original.restart, "{}", original.path);
        if original.path.starts_with("memory.") {
            assert_eq!(presented.tab, "Memory", "{}", original.path);
        }
        if original.tab == "Improvement" {
            assert_eq!(presented.tab, "Learning", "{}", original.path);
        }
    }
    for (path, group) in [
        ("providers", "Connections"),
        ("endpoint", "Connections"),
        ("mcp", "Connections"),
        ("approval", "Safety"),
        ("sandbox.mode", "Safety"),
        ("sandbox.extra_write", "Safety"),
        ("sandbox.network_allow", "Safety"),
        ("isolation.backend", "Safety"),
        ("secrets.values", "Safety"),
        ("shell.backend", "Tools"),
        ("keys", "Keys"),
    ] {
        let def = registry().iter().find(|def| def.path == path).unwrap();
        assert_eq!(def.tab, group, "{path}");
    }
}

#[test]
fn goal_search_terms_find_settings_without_reading_configured_values() {
    for (goal, path) in [
        ("stay offline", "sandbox.allow_network"),
        ("spend less", "budget"),
        ("stop asking", "approval"),
        ("notifications", "ui.notify_errors"),
        ("remember", "memory.backend"),
        ("learning", "improve.enabled"),
        ("connect", "providers"),
        ("hotkeys", "keys"),
    ] {
        let def = registry().iter().find(|def| def.path == path).unwrap();
        assert!(def.search_terms().contains(goal), "{goal}: {path}");
    }
    let providers = registry()
        .iter()
        .find(|def| def.path == "providers")
        .unwrap();
    assert!(providers.sensitive());
    assert!(!providers.search_terms().is_empty());
}

#[test]
fn web_timeout_keeps_the_historical_default_and_is_bounded() {
    let setting = registry()
        .iter()
        .find(|setting| setting.path == "web.timeout_secs")
        .expect("web timeout is a setting");
    assert_eq!(setting.default_value(), Some(Value::Integer(30)));
    assert_eq!(Config::default().web.timeout_secs, 30);
    let omitted: Config = toml::from_str("[web]\nsearch_provider = \"brave\"\n").unwrap();
    assert_eq!(omitted.web.timeout_secs, 30);
    check_value("web.timeout_secs", &Value::Integer(600)).unwrap();
    assert!(check_value("web.timeout_secs", &Value::Integer(0)).is_err());
    assert!(check_value("web.timeout_secs", &Value::Integer(601)).is_err());
    for seconds in [0, 601] {
        let mut config = Config::default();
        config.web.timeout_secs = seconds;
        assert!(config.validate().is_err(), "{seconds}");
    }
}

#[test]
fn failed_turns_notify_by_default_but_an_explicit_opt_out_is_kept() {
    assert!(UiConfig::default().notify_errors);
    let unset: Config = toml::from_str("[ui]\nnotify = true\n").unwrap();
    assert!(unset.ui.notify_errors);
    let opted_out: Config = toml::from_str("[ui]\nnotify_errors = false\n").unwrap();
    assert!(!opted_out.ui.notify_errors);
}

#[test]
fn reasoning_strategy_is_opt_in_and_all_settings_options_roundtrip() {
    use crate::config::types::ReasoningStrategy;
    let path = "experimental.reasoning_strategy";
    for source in [
        "",
        "[experimental]\n",
        "[experimental]\nvideo_analysis = true\n",
    ] {
        let config: Config = toml::from_str(source).unwrap();
        assert_eq!(
            config.experimental.reasoning_strategy,
            ReasoningStrategy::Default,
            "{source}"
        );
    }
    let setting = registry()
        .iter()
        .find(|setting| setting.path == path)
        .expect("reasoning strategy is registered");
    assert_eq!(
        (setting.tab, setting.section),
        ("Experimental", "Reasoning")
    );
    assert_eq!(
        setting.default_value(),
        Some(Value::String("default".into()))
    );
    let Kind::Enum(options) = setting.kind else {
        panic!("reasoning strategy must be a closed enum")
    };
    let expected = [
        ("default", ReasoningStrategy::Default),
        ("gpt_goal_directed", ReasoningStrategy::GptGoalDirected),
        ("claude_grounded", ReasoningStrategy::ClaudeGrounded),
        ("gemini_synthesis", ReasoningStrategy::GeminiSynthesis),
        (
            "deepseek_cross_check",
            ReasoningStrategy::DeepseekCrossCheck,
        ),
        ("evidence_first", ReasoningStrategy::EvidenceFirst),
        ("hypothesis_driven", ReasoningStrategy::HypothesisDriven),
        ("plan_then_act", ReasoningStrategy::PlanThenAct),
        ("decompose", ReasoningStrategy::Decompose),
        (
            "compare_alternatives",
            ReasoningStrategy::CompareAlternatives,
        ),
        ("critique_and_refine", ReasoningStrategy::CritiqueAndRefine),
        ("verification_first", ReasoningStrategy::VerificationFirst),
        ("minimal_change", ReasoningStrategy::MinimalChange),
    ];
    assert_eq!(
        options.iter().map(|(raw, _)| *raw).collect::<Vec<_>>(),
        expected.iter().map(|(raw, _)| *raw).collect::<Vec<_>>()
    );
    for (raw, strategy) in expected {
        let value = Value::String(raw.into());
        check_value(path, &value).unwrap();
        let mut settings = default_table();
        set(&mut settings, path, value.clone());
        let config: Config = settings.try_into().unwrap();
        config.validate().unwrap();
        assert_eq!(config.experimental.reasoning_strategy, strategy, "{raw}");
        let loaded: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(loaded.experimental.reasoning_strategy, strategy, "{raw}");
        assert_eq!(
            get(&Table::try_from(loaded).unwrap(), path),
            Some(&value),
            "{raw}"
        );
    }
    for invalid in [Value::String("unknown".into()), Value::Boolean(true)] {
        assert!(check_value(path, &invalid).is_err(), "accepted {invalid}");
        let mut settings = default_table();
        set(&mut settings, path, invalid.clone());
        assert!(
            Value::Table(settings).try_into::<Config>().is_err(),
            "deserialized {invalid}"
        );
    }
}

#[test]
fn batch_result_percent_defaults_to_thirty_zero_disables_and_is_bounded() {
    let setting = registry()
        .iter()
        .find(|setting| setting.path == "efficiency.batch_result_percent")
        .expect("the per-reply result budget is a setting");
    assert_eq!(setting.default_value(), Some(Value::Integer(30)));
    assert_eq!(Config::default().efficiency.batch_result_percent, 30);
    let omitted: Config = toml::from_str("[efficiency]\ntask_packet = true\n").unwrap();
    assert_eq!(omitted.efficiency.batch_result_percent, 30);
    check_value("efficiency.batch_result_percent", &Value::Integer(0)).unwrap();
    check_value("efficiency.batch_result_percent", &Value::Integer(80)).unwrap();
    assert!(check_value("efficiency.batch_result_percent", &Value::Integer(81)).is_err());
    for (percent, valid) in [(0, true), (80, true), (81, false)] {
        let mut config = Config::default();
        config.efficiency.batch_result_percent = percent;
        assert_eq!(config.validate().is_ok(), valid, "{percent}");
    }
}

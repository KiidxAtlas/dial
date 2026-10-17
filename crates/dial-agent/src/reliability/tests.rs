use super::*;
use crate::llm::reliability::{constrain_openai, omit_optional_nulls, strict_parameters};
#[test]
fn renders_edit_verification_progress() {
    let rendered = render(&json!({
        "enabled":true,
        "session":{
            "stats":{},
            "edit_loop":{
                "phase":"checks_passed_diff_surfaced_review_pending",
                "mutations":2,
                "checked_mutations":2,
                "diff_surfaced_mutations":1,
                "failed_checks":["related Rust tests: api::parser::tests::"],
                "uncheckpointed_shell_commands":1,
                "disclosed_shell_commands":0,
                "uncheckpointed_extension_calls":2,
                "disclosed_extension_calls":1,
                "uncheckpointed_extension_tools":["mcp__fixture"]
            }
        }
    }));
    assert!(rendered.contains(
        "Edit loop: checks_passed_diff_surfaced_review_pending · 2 edits · 2 checked · 1 diffs surfaced · 1 shell commands (0 disclosed) · 2 extension calls (1 disclosed)"
    ));
    assert!(rendered.contains("Failed focused checks: related Rust tests: api::parser::tests::"));
    assert!(rendered.contains("Uncheckpointed extension tools: mcp__fixture"));
}

#[test]
fn capability_deadline_aborts_incomplete_reply_without_scoring_it() {
    use std::{
        sync::{Arc, atomic::AtomicBool},
        time::Duration,
    };
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Stall {
        silence: Duration::from_millis(250),
    }]);
    let config = AppConfig::default();
    let mut backend = llm::provider::Backend::new(
        llm::provider::ProviderKind::Endpoint {
            base_url: server.url.clone(),
            model: "fixture".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    let chat = llm::Chat {
        messages: vec![json!({"role":"user","content":"fixture"})],
        tools: vec![],
        temperature: 0.,
        max_tokens: 32,
        seed: None,
    };
    let started = Instant::now();
    let error = calibration_chat_with_deadline(
        &config,
        &mut backend,
        &chat,
        &Arc::new(AtomicBool::new(false)),
        Duration::from_millis(20),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("previous measured profile preserved"),
        "{error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(server.requests().len(), 1);
}
#[test]
fn compact_interface_preserves_loaded_specialists_and_discovery() {
    let mut schemas = vec![
        json!({"function":{"name":"tool_load"}}),
        json!({"function":{"name":"read"}}),
        json!({"function":{"name":"sdk_query"}}),
        json!({"function":{"name":"unused"}}),
    ];
    simplify(&mut schemas, &HashSet::from(["sdk_query".to_owned()]));
    let contract = Contract::new(schemas).unwrap();
    assert_eq!(contract.names, ["tool_load", "read", "sdk_query"]);
}
#[test]
fn request_contract_is_exact_and_examples_validate() {
    let schemas = tools::schemas(&["read", "grep"], EditMode::Replace).unwrap();
    let contract = Contract::new(schemas.clone()).unwrap();
    assert_eq!(contract.names, ["read", "grep"]);
    let text = contract.instruction(true, true);
    assert!(text.contains("at most one tool call"));
    assert!(text.contains("read argument shape"));
    assert!(!text.contains("todo argument shape"));
    assert!(Contract::new(vec![schemas[0].clone(), schemas[0].clone()]).is_err());
}
#[test]
fn strict_optional_nulls_do_not_relax_required_or_unknown_fields() {
    // Strict optional-field conversion is tested on the single-path form. Public read also
    // supports `paths`, so `path` alone is no longer a globally required property.
    let mut schema =
        tools::schemas(&["read"], EditMode::Replace).unwrap()[0]["function"]["parameters"].clone();
    schema["required"] = json!(["path"]);
    let schema = &schema;
    let strict = strict_parameters(schema).unwrap();
    assert!(
        strict["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "limit")
    );
    assert_eq!(
        strict["properties"]["limit"]["type"],
        json!(["integer", "null"])
    );
    let mut args = json!({"path":"fixture.py","limit":null});
    omit_optional_nulls(&mut args, schema);
    assert_eq!(args, json!({"path":"fixture.py"}));
    for mut args in [
        json!({"path":null}),
        json!({"path":"fixture.py","unknown":null}),
        json!({"path":"fixture.py","limit":0}),
    ] {
        omit_optional_nulls(&mut args, schema);
        assert!(crate::agent::calls::validate(schema, &args).is_err());
    }
}
#[test]
fn strict_generation_requires_explicit_support_and_skips_complex_schemas() {
    let mut body = json!({"tools":tools::schemas(&["read"],EditMode::Replace).unwrap()});
    let mut profile = llm::profiles::ModelProfile::default();
    constrain_openai(&mut body, &profile);
    assert!(body["tools"][0]["function"].get("strict").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
    profile.capabilities.strict_tools = true;
    profile.capabilities.parallel_tool_calls = Some(false);
    constrain_openai(&mut body, &profile);
    assert_eq!(body["tools"][0]["function"]["strict"], true);
    assert_eq!(body["parallel_tool_calls"], false);
    assert!(
        strict_parameters(
            &json!({"type":"object","oneOf":[],"additionalProperties":false,"properties":{}})
        )
        .is_none()
    );
}
#[test]
fn tool_free_maintenance_requests_do_not_gain_tool_execution_controls() {
    let mut profile = llm::profiles::ModelProfile::default();
    profile.capabilities.strict_tools = true;
    profile.capabilities.parallel_tool_calls = Some(false);
    for mut body in [json!({"tools":[]}), json!({})] {
        let before = body.clone();
        constrain_openai(&mut body, &profile);
        assert_eq!(body, before);
    }
}
#[test]
fn recovery_counts_failed_rounds_and_only_progress_resets_them() {
    let mut config = AppConfig::default();
    // Single-call mode is an opt-in adaptation for models that cannot batch tool calls.
    assert!(!single_call(&config, None, &Stats::default()));
    config.reliability.measured_interfaces = true;
    let mut stats = Stats::default();
    for _ in 0..2 {
        stats.observe(true, false, false, 12);
        stats.round(true, false);
    }
    assert!(single_call(&config, None, &stats));
    stats.round(false, false);
    assert_eq!(stats.consecutive_contract_failures, 2);
    stats.observe(false, false, true, 25);
    stats.round(false, true);
    assert_eq!(stats.corrections, 1);
    assert_eq!(stats.first_useful_action_ms, Some(25));
    assert_eq!(stats.consecutive_contract_failures, 0);
    assert!(!single_call(&config, None, &stats));
    assert!(single_call(
        &config,
        Some(&json!({"batch_passed":false})),
        &stats
    ));
}
#[test]
fn measurement_is_bound_to_model_backend_and_edit_contract() {
    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig {
        state_dir: root.path().into(),
        ..Default::default()
    };
    config.edit.mode = EditMode::Replace;
    let data = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":binding(&config),"batch_passed":false});
    std::fs::write(
        root.path().join("reliability.json"),
        serde_json::to_vec(&data).unwrap(),
    )
    .unwrap();
    assert!(measured(&config).is_some());
    config.model.push_str("-changed");
    assert!(measured(&config).is_none());
    config.model = config.model.trim_end_matches("-changed").into();
    config.edit.mode = EditMode::Hashline;
    assert!(measured(&config).is_none());
}
#[test]
fn busy_report_uses_session_snapshot_without_loading_saved_profile() {
    let config = AppConfig::default();
    let control = json!({"reliability":{"measured":{"passed":4,"total":7},"stats":{"calls":2,"successes":1}}});
    let value = live_report(&config, &control);
    assert_eq!(value["measured"]["passed"], 4);
    assert!(render(&value).contains("1 successful / 2 calls"));
}

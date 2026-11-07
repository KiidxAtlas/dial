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
    // The stall outlasts the budget by far, and the budget is far above loopback connect latency,
    // so the request reaches the server before the deadline even on a loaded machine.
    let server = crate::llm::fake::serve(vec![crate::llm::fake::Script::Stall {
        silence: Duration::from_secs(3),
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
        Duration::from_millis(300),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("previous measured profile preserved"),
        "{error:#}"
    );
    // Aborted by the deadline, not by the server closing the stalled stream.
    assert!(started.elapsed() < Duration::from_millis(2500));
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
    assert!(text.starts_with("<dial-notice kind=\"tool_contract\">\nCall only: read, grep."));
    assert!(text.ends_with("\n</dial-notice>"));
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
        assert!(crate::agent::run::calls::validate(schema, &args).is_err());
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
#[test]
fn batch_permission_and_single_call_agree_on_partial_fixtures() {
    let mut config = AppConfig::default();
    config.reliability.measured_interfaces = true;
    for (record, permitted) in [
        (json!({"batch_passed":true,"passed":6,"total":6}), true),
        (json!({"batch_passed":true,"passed":5,"total":6}), false),
        (json!({"batch_passed":false,"passed":6,"total":6}), false),
    ] {
        assert_eq!(batch_permitted(&record), permitted, "{record}");
        assert_eq!(
            single_call(&config, Some(&record), &Stats::default()),
            !permitted,
            "{record}"
        );
    }
}
#[test]
fn route_report_shows_only_the_redacted_hosted_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        state_dir: root.path().into(),
        endpoint: Some(
            serde_json::from_value(json!({
                "base_url": "https://user:pass@api.example.com/v1?key=secret"
            }))
            .unwrap(),
        ),
        ..Default::default()
    };
    let value = route(&config);
    assert_eq!(value["endpoint"], "https://api.example.com/v1");
    assert_eq!(value["local"], false);
    assert!(value["server"].is_null(), "hosted routes are never asked");
    let text = value.to_string();
    assert!(
        !text.contains("secret") && !text.contains("user:pass"),
        "{text}"
    );
    let rendered = render(&json!({"enabled":true,"route":value}));
    assert!(
        rendered.contains("endpoint https://api.example.com/v1"),
        "{rendered}"
    );
    assert!(!rendered.contains("secret"), "{rendered}");
}
#[test]
fn binding_changes_with_context_window_and_sampling() {
    let root = tempfile::tempdir().unwrap();
    let mut config = AppConfig {
        state_dir: root.path().into(),
        ..Default::default()
    };
    let data = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":binding(&config),"batch_passed":true,"passed":7,"total":7});
    std::fs::write(
        root.path().join("reliability.json"),
        serde_json::to_vec(&data).unwrap(),
    )
    .unwrap();
    assert!(measured(&config).is_some());
    config.context_window += 1;
    assert!(
        measured(&config).is_none(),
        "a changed window is a new binding"
    );
    config.context_window -= 1;
    config.assistance.local.sampling = crate::config::types::LocalSamplingSource::Harness;
    assert!(
        measured(&config).is_none(),
        "a changed sampling source is a new binding"
    );
}
#[test]
fn session_snapshot_uses_the_turn_record_and_interface_without_probing() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        state_dir: root.path().into(),
        ..Default::default()
    };
    let record = json!({"batch_passed":true,"passed":7,"total":7,"cases":[]});
    let value = route_snapshot(&config, 4096, Some(&record), true);
    assert_eq!(value["selected_interface"], "one_call");
    assert_eq!(value["selected_interface_source"], "this session");
    assert_eq!(value["context_window"]["effective"], 4096);
    assert_eq!(value["server_probed"], false);
    assert!(value["server"].is_null());
    assert_eq!(
        route_snapshot(&config, 4096, Some(&record), false)["selected_interface"],
        "batch"
    );
    assert!(route_snapshot(&config, 4096, None, false)["measured"].is_null());
}
#[test]
fn live_report_reads_only_the_session_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        state_dir: root.path().into(),
        ..Default::default()
    };
    // A saved record that a live report must not read.
    let saved = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":binding(&config),"batch_passed":true,"passed":9,"total":9});
    std::fs::write(
        root.path().join("reliability.json"),
        serde_json::to_vec(&saved).unwrap(),
    )
    .unwrap();
    let control = json!({"reliability":{"measured":{"passed":4,"total":7},"route":{"selected_interface":"batch"}}});
    let value = live_report(&config, &control);
    assert_eq!(value["measured"]["passed"], 4);
    assert_eq!(value["route"]["selected_interface"], "batch");
}
#[test]
fn only_a_failed_or_text_lifted_non_batch_case_is_a_native_failure() {
    let failed = json!({"cases":[{"tool":"read","passed":true},{"tool":"edit","passed":false}]});
    assert!(native_failure(&failed));
    let batch_only =
        json!({"cases":[{"tool":"read","passed":true},{"tool":"batch","passed":false}]});
    assert!(!native_failure(&batch_only));
    let lifted = json!({"cases":[{"tool":"read","passed":true,"native":false,"text_lifted":true}]});
    assert!(
        native_failure(&lifted),
        "text-lifted calls are not native parsing"
    );
}

#[test]
fn request_call_policy_explains_measured_constraints_and_rendering() {
    let mut config = AppConfig::default();
    config.reliability.enabled = true;
    config.reliability.measured_interfaces = true;
    let mut stats = Stats::default();
    let passed = json!({"batch_passed":true,"passed":2,"total":2});
    let failed = json!({"batch_passed":false});
    assert!(single_call_reasons(&config, Some(&passed), &stats).is_empty());
    assert_eq!(
        single_call_reasons(&config, Some(&failed), &stats),
        ["saved_reliability_record"]
    );
    stats.consecutive_contract_failures = 2;
    assert_eq!(
        single_call_reasons(&config, Some(&failed), &stats),
        ["contract_recovery", "saved_reliability_record"]
    );
    config.reliability.enabled = false;
    assert!(single_call_reasons(&config, Some(&failed), &stats).is_empty());
    let rendered = render(
        &json!({"session":{"call_policy":{"mode":"one_call","reasons":["contract_recovery"]}}}),
    );
    assert!(
        rendered.contains("Request call policy: one_call · reasons: contract_recovery"),
        "{rendered}"
    );
}

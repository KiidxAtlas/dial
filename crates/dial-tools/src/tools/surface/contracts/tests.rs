use super::*;
fn schema(kind: &str) -> Value {
    json!({"type":"function","function":{"name":"test","parameters":{"type":"object","properties":{"value":{"type":kind}}}}})
}
#[test]
fn changing_native_edit_dialect_preserves_settled_history_without_replaying_it() {
    let registry = Registry::new();
    let old = crate::tools::schema("edit", crate::config::types::EditMode::Hashline).unwrap();
    let new = crate::tools::schema("edit", crate::config::types::EditMode::Replace).unwrap();
    let identity = identities(&[old], &registry)["edit"].clone();
    let history = vec![
        json!({"role":"assistant", "tool_calls":[{"id":"call", "type":"function", "function":{"name":"edit", "arguments":"{\"input\":\"historical hashline edit\"}"}}], "dial_tool_contracts":{"call":identity}}),
        json!({"role":"tool", "tool_call_id":"call", "content":"actual recorded edit result"}),
    ];
    assert!(
        project_history(&history, &[new], &registry)
            .unwrap()
            .is_none(),
        "compatible history needs no projection copy"
    );
}
#[test]
fn pure_revision_projection_is_idempotent_and_never_rewrites_or_executes_history() {
    struct Lift;
    impl crate::tools::integrations::ext::Tool for Lift {
        fn name(&self) -> &str {
            "test"
        }
        fn schema(&self) -> Value {
            schema("integer")
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn contract_revision(&self) -> u32 {
            2
        }
        fn call(
            &self,
            _: &crate::tools::integrations::ext::ToolContext,
            _: &Value,
        ) -> crate::tools::ToolOutput {
            panic!("projection must never execute")
        }
        fn project_historical_call(
            &self,
            old: &Identity,
            call: &HistoricalCall,
        ) -> Option<HistoricalCall> {
            if old != &identities(&[schema("string")], &Registry::new())["test"] {
                return None;
            }
            Some(HistoricalCall {
                arguments: json!({"value":call.arguments["value"].as_str()?.parse::<u64>().ok()?}),
                output: format!("revision 2: {}", call.output),
            })
        }
    }
    let old = identities(&[schema("string")], &Registry::new());
    let history = vec![
        json!({"role":"assistant", "tool_calls":[{"id":"a", "type":"function", "function":{"name":"test", "arguments":"{\"value\":\"7\"}"}}], "dial_tool_contracts":{"a":old["test"]}}),
        json!({"role":"tool", "tool_call_id":"a", "content":"kept result"}),
    ];
    let original = history.clone();
    let mut registry = Registry::new();
    registry.register(std::sync::Arc::new(Lift));
    let projected = project_history(&history, &[schema("integer")], &registry)
        .unwrap()
        .expect("a revised historical call must be projected");
    assert_eq!(
        projected[0]["tool_calls"][0]["function"]["arguments"],
        "{\"value\":7}"
    );
    assert_eq!(projected[1]["content"], "revision 2: kept result");
    assert!(
        project_history(&projected, &[schema("integer")], &registry)
            .unwrap()
            .is_none(),
        "an already projected history is idempotent without another copy"
    );
    assert_eq!(history, original);
}
#[test]
fn schema_annotations_do_not_erase_property_names_or_literal_values() {
    let schema = json!({
        "description":"help",
        "properties":{
            "description":{"type":"string", "description":"parameter help"},
            "payload":{"const":{"description":"literal data"}}
        },
        "anyOf":[{"title":"label", "required":["description"]}]
    });
    assert_eq!(
        parameter_shape(&schema),
        json!({
            "properties":{
                "description":{"type":"string"},
                "payload":{"const":{"description":"literal data"}}
            },
            "anyOf":[{"required":["description"]}]
        })
    );
}
#[test]
fn compatibility_requires_explicit_tool_attestation_and_preserves_history() {
    struct Compatible;
    impl crate::tools::integrations::ext::Tool for Compatible {
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn name(&self) -> &str {
            "test"
        }
        fn schema(&self) -> Value {
            schema("integer")
        }
        fn contract_revision(&self) -> u32 {
            2
        }
        fn accepts_historical_contract(&self, old: &Identity) -> bool {
            old == &identities(&[schema("string")], &Registry::new())["test"]
        }
        fn call(
            &self,
            _: &crate::tools::integrations::ext::ToolContext,
            _: &Value,
        ) -> crate::tools::ToolOutput {
            panic!("history must never execute")
        }
    }
    let old = identities(&[schema("string")], &Registry::new());
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old["test"]}})];
    let original = history.clone();
    let mut registry = Registry::new();
    registry.register(std::sync::Arc::new(Compatible));
    validate_history(&history, &[schema("integer")], &registry).unwrap();
    assert_eq!(history, original);
    let mut unsupported = history;
    unsupported[0]["dial_tool_contracts"]["id"]["revision"] = json!(99);
    assert!(validate_history(&unsupported, &[schema("integer")], &registry).is_err());
}
#[test]
fn changed_schema_refuses_continuation_but_legacy_and_documentation_remain_readable() {
    let registry = Registry::new();
    let old = identities(&[schema("string")], &registry);
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old["test"]}})];
    validate_history(&history, &[schema("string")], &registry).unwrap();
    assert!(validate_history(&history, &[schema("integer")], &registry).is_err());
    let mut documented = schema("string");
    documented["function"]["description"] = json!("new help");
    documented["function"]["parameters"]["properties"]["value"]["description"] =
        json!("new parameter help");
    validate_history(&history, &[documented], &registry).unwrap();
    validate_history(
        &[json!({"role":"assistant","tool_calls":[]})],
        &[schema("integer")],
        &registry,
    )
    .unwrap();
    validate_history(&history, &[], &registry).unwrap();
}
#[test]
fn history_recorded_before_an_additive_builtin_property_stays_valid() {
    let registry = Registry::new();
    let current = crate::tools::schema("grep", crate::config::types::EditMode::Replace).unwrap();
    let mut before = current.clone();
    before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("output");
    let old = identities(&[before], &registry)["grep"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
    let mut changed = current;
    changed["function"]["parameters"]["properties"]["pattern"]["type"] = json!("integer");
    let mut other = history;
    other[0]["dial_tool_contracts"]["id"] = json!(identities(&[changed], &registry)["grep"]);
    assert!(
        validate_history(
            &other,
            &[crate::tools::schema("grep", crate::config::types::EditMode::Replace).unwrap()],
            &registry
        )
        .is_err()
    );
}

/// `edit` gained the optional `allow_syntax_errors`; sessions recorded before it resume in every
/// edit grammar.
#[test]
fn edit_history_recorded_before_allow_syntax_errors_stays_valid() {
    use crate::config::types::EditMode;
    let registry = Registry::new();
    for mode in [
        EditMode::Hashline,
        EditMode::Replace,
        EditMode::Patch,
        EditMode::ApplyPatch,
        EditMode::Sloppy,
    ] {
        let current = crate::tools::schema("edit", mode).unwrap();
        let mut before = current.clone();
        assert!(
            before["function"]["parameters"]["properties"]
                .as_object_mut()
                .unwrap()
                .remove("allow_syntax_errors")
                .is_some(),
            "{mode:?}"
        );
        let old = identities(&[before], &registry)["edit"].clone();
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{"edit":old}})];
        validate_history(&history, &[current], &registry)
            .unwrap_or_else(|e| panic!("{mode:?}: {e}"));
    }
}

// The controller owns this schema (dial-agent cannot be a dial-tools dependency). Keep the
// predecessor literal so changes to existing fields cannot silently redefine compatibility.
fn check_schema_before_repro() -> Value {
    json!({"type":"function","function":{"name":"check","parameters":{
        "type":"object",
        "properties":{
            "names":{"type":"array","maxItems":16,"items":{"type":"string"}},
            "failed_only":{"type":"boolean"}
        },
        "additionalProperties":false
    }}})
}

fn check_schema_with_repro() -> Value {
    let mut schema = check_schema_before_repro();
    schema["function"]["parameters"]["properties"]["repro"] = json!({
        "type":"object",
        "properties":{"name":{"type":"string"},"command":{"type":"string"}},
        "required":["name","command"],
        "additionalProperties":false
    });
    schema
}

#[test]
fn check_history_before_repro_preserves_saved_calls_and_callback_catalogs() {
    let registry = Registry::new();
    let old = identities(&[check_schema_before_repro()], &registry)["check"].clone();
    let histories = [
        vec![
            json!({"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"check","arguments":"{\"failed_only\":true}"}}],"dial_tool_contracts":{"call":old}}),
            json!({"role":"tool","tool_call_id":"call","content":"recorded check result"}),
        ],
        vec![json!({"role":"assistant","dial_tool_contracts":{"callback:check":old}})],
    ];
    for history in histories {
        let original = history.clone();
        assert!(
            project_history(&history, &[check_schema_with_repro()], &registry)
                .unwrap()
                .is_none(),
            "compatible history needs no projection copy"
        );
        assert_eq!(history, original);
    }
}

fn task_schema(actions: &[&str], pool_fields: bool) -> Value {
    let mut properties = json!({
        "action": {"type": "string", "enum": actions},
        "task": {"type": "string"}
    });
    if pool_fields {
        properties["concurrency"] = json!({"type": "integer", "minimum": 1, "maximum": 32});
    }
    json!({"type": "function", "function": {"name": "task", "strict": false,
        "parameters": {"type": "object", "properties": properties, "required": ["action"]}}})
}

#[test]
fn task_history_before_pool_actions_resumes_but_other_task_changes_are_rejected() {
    let registry = Registry::new();
    let prior = ["run", "start", "resume", "status", "wait", "cancel"];
    let pooled = [
        "run", "start", "resume", "status", "wait", "cancel", "create", "push", "peek", "close",
    ];
    let old = identities(&[task_schema(&prior, false)], &registry)["task"].clone();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"task","arguments":"{\"action\":\"resume\",\"id\":\"task-1\"}"}}],"dial_tool_contracts":{"call":old}}),
        json!({"role":"tool","tool_call_id":"call","content":"recorded resume result"}),
    ];
    assert!(
        project_history(&history, &[task_schema(&pooled, true)], &registry)
            .unwrap()
            .is_none(),
        "pool actions and concurrency are compatible with prior task history"
    );
    let mut unrelated = task_schema(&pooled, true);
    unrelated["function"]["parameters"]["properties"]["output_schema"] = json!({"type": "object"});
    assert!(
        project_history(&history, &[unrelated], &registry).is_err(),
        "an unrelated task schema change must still reject prior history"
    );
}

/// `check.expect` comes and goes with `experimental.check_predictions`; history recorded on
/// either side of the flag stays valid, while other changes are still refused.
#[test]
fn check_prediction_property_is_compatible_in_both_directions() {
    let registry = Registry::new();
    let mut with_expect = check_schema_with_repro();
    with_expect["function"]["parameters"]["properties"]["expect"] = check_prediction_property();
    let schemas = [
        check_schema_before_repro(),
        check_schema_with_repro(),
        with_expect.clone(),
    ];
    for recorded in &schemas {
        let old = identities(std::slice::from_ref(recorded), &registry)["check"].clone();
        let history =
            vec![json!({"role":"assistant","dial_tool_contracts":{"callback:check":old}})];
        for current in [check_schema_with_repro(), with_expect.clone()] {
            assert!(validate_history(&history, &[current], &registry).is_ok());
        }
    }
    let old = identities(&[with_expect.clone()], &registry)["check"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"callback:check":old}})];
    let mut changed = with_expect;
    changed["function"]["parameters"]["properties"]["expect"]["enum"] = json!(["pass"]);
    assert!(
        validate_history(&history, &[changed], &registry).is_err(),
        "a different expect shape is not the additive property"
    );
}

#[test]
fn check_repro_compatibility_rejects_nonadditive_contract_changes() {
    let registry = Registry::new();
    let old = identities(&[check_schema_before_repro()], &registry)["check"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"callback:check":old}})];
    let current = check_schema_with_repro();
    let mut changes = Vec::new();
    let mut changed = current.clone();
    changed["function"]["parameters"]["properties"]["names"]["items"]["type"] = json!("integer");
    changes.push(changed);
    let mut changed = current.clone();
    changed["function"]["parameters"]["required"] = json!(["repro"]);
    changes.push(changed);
    let mut changed = current.clone();
    changed["function"]["parameters"]["additionalProperties"] = json!(true);
    changes.push(changed);
    let mut changed = current.clone();
    changed["function"]["strict"] = json!(true);
    changes.push(changed);
    let mut changed = current.clone();
    changed["function"]["parameters"]["properties"]["another_option"] = json!({"type":"boolean"});
    changes.push(changed);
    for changed in changes {
        assert!(validate_history(&history, &[changed], &registry).is_err());
    }
    let mut unknown_revision = history.clone();
    unknown_revision[0]["dial_tool_contracts"]["callback:check"]["revision"] = json!(2);
    assert!(validate_history(&unknown_revision, &[current], &registry).is_err());
}

/// `web_search` gained provider names in its `provider` enum; sessions recorded before them resume.
#[test]
fn web_search_history_recorded_before_added_providers_stays_valid() {
    use crate::tools::integrations::ext::Tool;
    let tool = crate::tools::net::web::search_tool_for_contract_tests();
    let current = tool.schema();
    let names: Vec<String> = current["function"]["parameters"]["properties"]["provider"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    for added in [
        "firecrawl",
        "parallel",
        "zai",
        "xai",
        "startpage",
        "mojeek",
        "ecosia",
    ] {
        assert!(names.iter().any(|name| name == added), "{added}");
    }
    let mut before = current.clone();
    before["function"]["parameters"]["properties"]["provider"]["enum"] = json!(&names[..12]);
    // History recorded before the added providers also predates the domain filters.
    before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .retain(|name, _| name != "domains" && name != "exclude_domains");
    let mut registry = Registry::new();
    registry.register(std::sync::Arc::new(tool));
    let old = identities(&[before.clone()], &Registry::new())["web_search"].clone();
    assert_ne!(
        old,
        identities(std::slice::from_ref(&current), &Registry::new())["web_search"]
    );
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();

    let mut changed = before;
    changed["function"]["parameters"]["properties"]["query"]["type"] = json!("integer");
    let mut other = history;
    other[0]["dial_tool_contracts"]["id"] =
        json!(identities(&[changed], &Registry::new())["web_search"]);
    assert!(validate_history(&other, &[current], &registry).is_err());
}

/// `kernel` became `eval` with `language:"py"`. Recorded Python cells resume as eval calls.
#[test]
fn recorded_kernel_cells_project_onto_eval_as_python() {
    let registry = Registry::new();
    let kernel = json!({"type":"function","function":{"name":"kernel","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["eval","reset","cancel","status"]},"code":{"type":"string"},"timeout":{"type":"integer"}}}}});
    let old = identities(&[kernel], &registry)["kernel"].clone();
    let current = crate::tools::exec::eval::tools(&crate::config::types::Config::default())
        .pop()
        .unwrap()
        .schema();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"kernel","arguments":"{\"action\":\"eval\",\"code\":\"1\"}"}}],"dial_tool_contracts":{"c1":old}}),
        json!({"role":"tool","tool_call_id":"c1","content":"1"}),
    ];
    let projected = project_history(&history, std::slice::from_ref(&current), &registry)
        .unwrap()
        .expect("a recorded kernel cell must be projected onto eval");
    let call = &projected[0]["tool_calls"][0]["function"];
    assert_eq!(call["name"], "eval");
    let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        arguments,
        json!({"action":"eval","code":"1","language":"py"})
    );
    assert!(
        project_history(&projected, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none(),
        "an already projected history is idempotent"
    );
    validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();
}

/// A legacy kernel call that already names a language is not the shape the rename produced.
#[test]
fn recorded_kernel_calls_with_an_explicit_language_are_refused() {
    let registry = Registry::new();
    let kernel = json!({"type":"function","function":{"name":"kernel","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["eval","reset","cancel","status"]},"code":{"type":"string"}}}}});
    let old = identities(&[kernel], &registry)["kernel"].clone();
    let current = crate::tools::exec::eval::tools(&crate::config::types::Config::default())
        .pop()
        .unwrap()
        .schema();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"kernel","arguments":"{\"action\":\"eval\",\"language\":\"js\",\"code\":\"1\"}"}}],"dial_tool_contracts":{"c1":old}}),
        json!({"role":"tool","tool_call_id":"c1","content":"1"}),
    ];
    assert!(project_history(&history, std::slice::from_ref(&current), &registry).is_err());
}

/// `eval` gained `language:"py"`; sessions recorded when only JavaScript existed stay valid.
#[test]
fn eval_history_recorded_before_python_language_stays_valid() {
    use crate::tools::integrations::ext::Tool;
    let tool = crate::tools::exec::eval::tools(&crate::config::types::Config::default())
        .pop()
        .unwrap();
    let current = tool.schema();
    let mut before = current.clone();
    before["function"]["parameters"]["properties"]["language"]["enum"] = json!(["js"]);
    before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .retain(|name, _| name != "background" && name != "title");
    let mut registry = Registry::new();
    registry.register(tool);
    let old = identities(&[before.clone()], &Registry::new())["eval"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"c1":old}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();

    let mut changed = before;
    changed["function"]["parameters"]["properties"]["code"]["type"] = json!("integer");
    let other = vec![json!({"role":"assistant","dial_tool_contracts":{"c1":
        identities(&[changed], &Registry::new())["eval"]}})];
    assert!(validate_history(&other, &[current], &registry).is_err());
}

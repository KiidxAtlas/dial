use super::*;
fn schema(kind: &str) -> Value {
    json!({"type":"function","function":{"name":"test","parameters":{"type":"object","properties":{"value":{"type":kind}}}}})
}

#[test]
fn data_io_and_scoped_checks_resume_old_builtin_contracts_without_replay() {
    let registry = Registry::new();
    for (name, added) in [
        ("read", &["format", "retain"][..]),
        ("write", &["encoding"][..]),
        ("bash", &["check_paths"][..]),
    ] {
        let current =
            crate::tools::schema(name, crate::config::types::EditMode::default()).unwrap();
        let mut old = current.clone();
        for property in added {
            old["function"]["parameters"]["properties"]
                .as_object_mut()
                .unwrap()
                .remove(*property)
                .unwrap();
        }
        let identity = identities(&[old], &registry)[name].clone();
        let history = vec![
            json!({"role":"assistant","dial_tool_contracts":{"call":identity},
            "tool_calls":[{"id":"call","type":"function","function":{"name":name,"arguments":"{}"}}]}),
        ];
        assert!(
            project_history(&history, &[current], &registry)
                .unwrap()
                .is_none(),
            "{name}: no replay or mutation of history"
        );
    }
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
fn glob_history_before_previews_remains_compatible() {
    let registry = Registry::new();
    let current = crate::tools::schema("glob", crate::config::types::EditMode::Hashline).unwrap();
    let mut before = current.clone();
    before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("preview");
    let identity = identities(&[before], &registry)["glob"].clone();
    let history = vec![json!({"role":"assistant", "dial_tool_contracts":{"call":identity}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
    let mut changed = current;
    changed["function"]["parameters"]["properties"]["pattern"]["type"] = json!("integer");
    assert!(validate_history(&history, &[changed], &registry).is_err());
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

#[test]
fn bash_history_before_named_sessions_stays_valid_without_rewriting_calls() {
    let registry = Registry::new();
    let current = crate::tools::schema("bash", crate::config::types::EditMode::Replace).unwrap();
    let mut before = current.clone();
    assert!(
        before["function"]["parameters"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("session")
            .is_some()
    );
    let old = identities(&[before], &registry)["bash"].clone();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}],"dial_tool_contracts":{"call":old}}),
        json!({"role":"tool","tool_call_id":"call","content":"recorded output"}),
    ];
    let original = history.clone();
    assert!(
        project_history(&history, &[current], &registry)
            .unwrap()
            .is_none()
    );
    assert_eq!(history, original);
}

#[test]
fn bash_history_before_explicit_checks_resumes_without_changing_recorded_calls() {
    let registry = Registry::new();
    let current = crate::tools::schema("bash", crate::config::types::EditMode::Replace).unwrap();
    let mut before = current.clone();
    before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("check");
    let identity = identities(&[before], &registry)["bash"].clone();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"old","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}],"dial_tool_contracts":{"old":identity}}),
        json!({"role":"tool","tool_call_id":"old","content":"recorded output"}),
    ];
    let original = history.clone();
    assert!(
        project_history(&history, &[current], &registry)
            .unwrap()
            .is_none()
    );
    assert_eq!(history, original);
}

#[test]
fn bash_history_before_pty_stays_valid_with_or_without_the_session_property() {
    let registry = Registry::new();
    let current = crate::tools::schema("bash", crate::config::types::EditMode::Replace).unwrap();
    for had_session in [false, true] {
        let mut before = current.clone();
        let properties = before["function"]["parameters"]["properties"]
            .as_object_mut()
            .unwrap();
        assert!(properties.remove("pty").is_some());
        if !had_session {
            assert!(properties.remove("session").is_some());
        }
        let old = identities(std::slice::from_ref(&before), &registry)["bash"].clone();
        let arguments = if had_session {
            json!({"command":"pwd", "session":"build"})
        } else {
            json!({"command":"pwd"})
        };
        let history = vec![
            json!({"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"bash","arguments":arguments.to_string()}}],"dial_tool_contracts":{"call":old}}),
            json!({"role":"tool","tool_call_id":"call","content":"recorded output"}),
        ];
        let original = history.clone();
        assert!(
            project_history(&history, std::slice::from_ref(&current), &registry)
                .unwrap()
                .is_none()
        );
        assert_eq!(history, original);
        before["function"]["parameters"]["properties"]["command"]["type"] = json!("integer");
        let mut incompatible = history;
        incompatible[0]["dial_tool_contracts"]["call"] =
            json!(identities(&[before], &registry)["bash"]);
        assert!(project_history(&incompatible, std::slice::from_ref(&current), &registry).is_err());
    }
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

#[test]
fn edit_history_before_optional_verify_stays_valid_in_every_grammar() {
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
        assert_eq!(
            before["function"]["parameters"]["properties"]
                .as_object_mut()
                .unwrap()
                .remove("verify")
                .unwrap()["type"],
            "array"
        );
        let old = identities(&[before], &registry)["edit"].clone();
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{"edit":old}})];
        validate_history(&history, &[current], &registry)
            .unwrap_or_else(|error| panic!("{mode:?}: {error}"));
    }
}

#[test]
fn write_and_ast_history_before_syntax_override_remains_compatible() {
    let registry = Registry::new();
    for current in [
        crate::tools::schema("write", crate::config::types::EditMode::default()).unwrap(),
        crate::tools::edit::ast::tools(&crate::config::types::Config::default())
            .into_iter()
            .find(|tool| tool.name() == "ast_edit")
            .unwrap()
            .schema(),
    ] {
        let name = current["function"]["name"].as_str().unwrap();
        let mut before = current.clone();
        assert!(
            before["function"]["parameters"]["properties"]
                .as_object_mut()
                .unwrap()
                .remove("allow_syntax_errors")
                .is_some(),
            "{name}"
        );
        let old = identities(&[before], &registry)[name].clone();
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{name:old}})];
        validate_history(&history, &[current], &registry).unwrap();
    }
}

// The controller owns this schema (dial-agent cannot be a dial-tools dependency). Keep the
// predecessor literal so changes to existing fields cannot silently redefine compatibility.
fn verify_schema_before_repro() -> Value {
    json!({"type":"function","function":{"name":"verify","parameters":{
        "type":"object",
        "properties":{
            "targets":{"type":"array","maxItems":16,"items":{"type":"string"}}
        },
        "additionalProperties":false
    }}})
}

fn verify_schema_with_repro() -> Value {
    let mut schema = verify_schema_before_repro();
    schema["function"]["parameters"]["properties"]["repro"] = json!({
        "type":"object",
        "properties":{"name":{"type":"string"},"command":{"type":"string"}},
        "required":["name","command"],
        "additionalProperties":false
    });
    schema
}

#[test]
fn verify_history_before_repro_preserves_saved_calls_and_callback_catalogs() {
    let registry = Registry::new();
    let old = identities(&[verify_schema_before_repro()], &registry)["verify"].clone();
    let histories = [
        vec![
            json!({"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"verify","arguments":"{\"targets\":[\"failed\"]}"}}],"dial_tool_contracts":{"call":old}}),
            json!({"role":"tool","tool_call_id":"call","content":"recorded verify result"}),
        ],
        vec![json!({"role":"assistant","dial_tool_contracts":{"callback:verify":old}})],
    ];
    for history in histories {
        let original = history.clone();
        assert!(
            project_history(&history, &[verify_schema_with_repro()], &registry)
                .unwrap()
                .is_none(),
            "compatible history needs no projection copy"
        );
        assert_eq!(history, original);
    }
}

#[test]
fn verify_plan_is_additive_without_reinterpreting_recorded_calls() {
    let registry = Registry::new();
    let before = verify_schema_with_repro();
    let old = identities(std::slice::from_ref(&before), &registry)["verify"].clone();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"verify","arguments":"{\"targets\":[\"failed\"]}"}}],"dial_tool_contracts":{"c1":old}}),
        json!({"role":"tool","tool_call_id":"c1","content":"recorded result"}),
    ];
    let original = history.clone();
    let mut current = before;
    current["function"]["parameters"]["properties"]["plan"] = json!({"type":"boolean"});
    assert!(
        project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none()
    );
    assert_eq!(history, original);
    current["function"]["parameters"]["properties"]["targets"]["items"]["type"] = json!("number");
    assert!(validate_history(&history, &[current], &registry).is_err());
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

#[test]
fn task_history_before_isolation_backend_resumes() {
    let registry = Registry::new();
    let pooled = [
        "run", "start", "resume", "status", "wait", "cancel", "create", "push", "peek", "close",
    ];
    let mut before = task_schema(&pooled, true);
    before["function"]["parameters"]["properties"]["tasks"] = json!({"type": "array",
        "items": {"type": "object", "properties": {"task": {"type": "string"}}}});
    let backend = json!({"type": "string", "enum": ["auto", "clone", "worktree", "copy"]});
    let mut current = before.clone();
    let properties = &mut current["function"]["parameters"]["properties"];
    properties["isolation_backend"] = backend.clone();
    properties["tasks"]["items"]["properties"]["isolation_backend"] = backend;
    // Recorded before `isolation_backend`, both with and without the pool actions.
    let mut before_pool = before.clone();
    before_pool["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("concurrency");
    before_pool["function"]["parameters"]["properties"]["action"]["enum"] =
        json!(["run", "start", "resume", "status", "wait", "cancel"]);
    for recorded in [before.clone(), before_pool] {
        let old = identities(&[recorded], &registry)["task"].clone();
        let history = vec![
            json!({"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"task","arguments":"{\"action\":\"status\"}"}}],"dial_tool_contracts":{"call":old}}),
            json!({"role":"tool","tool_call_id":"call","content":"recorded status"}),
        ];
        assert!(
            project_history(&history, std::slice::from_ref(&current), &registry)
                .unwrap()
                .is_none(),
            "isolation_backend is additive for prior task history"
        );
    }
    // Any other item change is still refused.
    before["function"]["parameters"]["properties"]["tasks"]["items"]["properties"]["isolation_backend"] =
        json!({"type": "integer"});
    let old = identities(&[before], &registry)["task"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"call":old}})];
    assert!(project_history(&history, &[current], &registry).is_err());
}

/// `verify.expect` comes and goes with `experimental.check_predictions`; history recorded on
/// either side of the flag stays valid, while other changes are still refused.
#[test]
fn verify_prediction_property_is_compatible_in_both_directions() {
    let registry = Registry::new();
    let mut with_expect = verify_schema_with_repro();
    with_expect["function"]["parameters"]["properties"]["expect"] = check_prediction_property();
    let schemas = [
        verify_schema_before_repro(),
        verify_schema_with_repro(),
        with_expect.clone(),
    ];
    for recorded in &schemas {
        let old = identities(std::slice::from_ref(recorded), &registry)["verify"].clone();
        let history =
            vec![json!({"role":"assistant","dial_tool_contracts":{"callback:verify":old}})];
        for current in [verify_schema_with_repro(), with_expect.clone()] {
            assert!(validate_history(&history, &[current], &registry).is_ok());
        }
    }
    let old = identities(&[with_expect.clone()], &registry)["verify"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"callback:verify":old}})];
    let mut changed = with_expect;
    changed["function"]["parameters"]["properties"]["expect"]["enum"] = json!(["pass"]);
    assert!(
        validate_history(&history, &[changed], &registry).is_err(),
        "a different expect shape is not the additive property"
    );
}

#[test]
fn verify_repro_compatibility_rejects_nonadditive_contract_changes() {
    let registry = Registry::new();
    let old = identities(&[verify_schema_before_repro()], &registry)["verify"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"callback:verify":old}})];
    let current = verify_schema_with_repro();
    let mut changes = Vec::new();
    let mut changed = current.clone();
    changed["function"]["parameters"]["properties"]["targets"]["items"]["type"] = json!("integer");
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
    unknown_revision[0]["dial_tool_contracts"]["callback:verify"]["revision"] = json!(2);
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
    // This provider set predates both the domain filters and optional result-page fetching.
    before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .retain(|name, _| !matches!(name.as_str(), "domains" | "exclude_domains" | "fetch_top"));
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

/// Consolidated members (`facade::GROUPS`) resume as calls of their successor with the selector
/// that routes back to them; an already-selected shape is refused.
#[test]
fn recorded_consolidated_member_calls_project_onto_their_successor() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::config::types::Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: Vec::new(),
        ..crate::config::types::Config::default()
    };
    let registry = Registry::standard(&config, Vec::new());
    let member = registry.get("runtime_inspect").unwrap();
    let old = identities(&[member.schema()], &registry)["runtime_inspect"].clone();
    let current = registry.get("inspect").unwrap().schema();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"runtime_inspect","arguments":"{\"action\":\"summary\",\"path\":\"t.json\"}"}}],"dial_tool_contracts":{"c1":old}}),
        json!({"role":"tool","tool_call_id":"c1","content":"ok"}),
    ];
    let projected = project_history(&history, std::slice::from_ref(&current), &registry)
        .unwrap()
        .expect("a recorded member call must be projected onto its successor");
    let call = &projected[0]["tool_calls"][0]["function"];
    assert_eq!(call["name"], "inspect");
    let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(
        arguments,
        json!({"kind":"trace","action":"summary","path":"t.json"})
    );
    assert!(
        project_history(&projected, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none()
    );
    validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();

    let mut selected = history;
    selected[0]["tool_calls"][0]["function"]["arguments"] =
        json!("{\"kind\":\"x\",\"action\":\"summary\"}");
    assert!(project_history(&selected, std::slice::from_ref(&current), &registry).is_err());
    // Without the successor in this request the call stays inert.
    assert!(
        project_history(&selected, &[], &registry)
            .unwrap()
            .is_none()
    );
}

/// The split memory tools merged into `memory`; a recorded call resumes as `memory` with the
/// action that selected it, and an already-actioned shape is refused.
#[test]
fn recorded_split_memory_calls_project_onto_memory_actions() {
    let registry = Registry::new();
    let update = json!({"type":"function","function":{"name":"memory_update","parameters":{"type":"object","properties":{"id":{"type":"integer"},"content":{"type":"string"}}}}});
    let old = identities(&[update], &registry)["memory_update"].clone();
    let current = json!({"type":"function","function":{"name":"memory","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["recall","update"]},"id":{"type":"integer"},"content":{"type":"string"}},"required":["action"]}}});
    let history = |arguments: &str| {
        vec![
            json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"memory_update","arguments":arguments}}],"dial_tool_contracts":{"c1":old}}),
            json!({"role":"tool","tool_call_id":"c1","content":"{}"}),
        ]
    };
    let projected = project_history(
        &history("{\"id\":3,\"content\":\"x\"}"),
        std::slice::from_ref(&current),
        &registry,
    )
    .unwrap()
    .expect("a recorded memory_update call must be projected onto memory");
    let call = &projected[0]["tool_calls"][0]["function"];
    assert_eq!(call["name"], "memory");
    let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments, json!({"action":"update","id":3,"content":"x"}));
    validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();
    assert!(
        project_history(
            &history("{\"action\":\"recall\"}"),
            std::slice::from_ref(&current),
            &registry
        )
        .is_err()
    );
}

/// The removed pre-split monolithic `memory` contract has no projection onto the current one.
#[test]
fn recorded_monolithic_memory_calls_are_refused() {
    let registry = Registry::new();
    let monolithic = json!({"type":"function","function":{"name":"memory","parameters":{"type":"object","properties":{"op":{"type":"string"},"text":{"type":"string"}}}}});
    let old = identities(&[monolithic], &registry)["memory"].clone();
    let current = json!({"type":"function","function":{"name":"memory","parameters":{"type":"object","properties":{"action":{"type":"string","enum":["recall"]},"query":{"type":"string"}},"required":["action"]}}});
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"memory","arguments":"{\"op\":\"add\",\"text\":\"x\"}"}}],"dial_tool_contracts":{"c1":old}}),
        json!({"role":"tool","tool_call_id":"c1","content":"{}"}),
    ];
    assert!(project_history(&history, std::slice::from_ref(&current), &registry).is_err());
}

/// `artifact_read` merged into `read`; each recorded mode resumes as the equivalent `artifact://`
/// path selector, and a shape the old tool could not express in a selector is refused.
#[test]
fn recorded_artifact_read_calls_project_onto_read_selectors() {
    let registry = Registry::new();
    let legacy = json!({"type":"function","function":{"name":"artifact_read","parameters":{"type":"object","properties":{"uri":{"type":"string"},"info":{"type":"boolean"},"annotations":{"type":"boolean"},"start":{"type":"integer"},"count":{"type":"integer"},"offset":{"type":"integer"},"bytes":{"type":"integer"}},"required":["uri"]}}});
    let old = identities(&[legacy], &registry)["artifact_read"].clone();
    let current = json!({"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}});
    let history = |arguments: Value| {
        vec![
            json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"artifact_read","arguments":arguments.to_string()}}],"dial_tool_contracts":{"c1":old}}),
            json!({"role":"tool","tool_call_id":"c1","content":"x"}),
        ]
    };
    let uri = format!("artifact://{}", "a".repeat(64));
    for (arguments, selector) in [
        (json!({"uri":uri}), "1+100"),
        (json!({"uri":uri,"start":40,"count":20}), "40+20"),
        (json!({"uri":uri,"info":true}), "info"),
        (json!({"uri":uri,"annotations":true}), "annotations"),
        (json!({"uri":uri,"offset":64,"bytes":32}), "bytes=64+32"),
        (json!({"uri":uri,"offset":0}), "bytes=0+8192"),
    ] {
        let projected = project_history(
            &history(arguments),
            std::slice::from_ref(&current),
            &registry,
        )
        .unwrap()
        .expect("a recorded artifact_read call must be projected onto read");
        let call = &projected[0]["tool_calls"][0]["function"];
        assert_eq!(call["name"], "read");
        let arguments: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments, json!({"path": format!("{uri}:{selector}")}));
        validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();
    }
    for refused in [
        json!({"uri":uri,"annotations":true,"start":2}),
        json!({"uri":uri,"extra":1}),
        json!({"start":1}),
    ] {
        assert!(
            project_history(&history(refused), std::slice::from_ref(&current), &registry).is_err()
        );
    }
}

/// `eval` gained `language:"py"`; sessions recorded when only JavaScript existed stay valid.
#[test]
fn eval_history_recorded_before_python_language_stays_valid() {
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

#[test]
fn document_inspection_enum_preserves_prior_layout_history() {
    let config = crate::config::types::Config::default();
    let tools = crate::tools::fs::documents::analysis_tools(&config);
    let current = tools[0].schema();
    let mut before = current.clone();
    before["function"]["parameters"]["properties"]["action"]["enum"] = json!(["layout", "compare"]);
    let mut registry = Registry::new();
    registry.register(std::sync::Arc::clone(&tools[0]));
    let old = identities(&[before], &registry)["document"].clone();
    let history = vec![json!({"role":"assistant", "dial_tool_contracts":{"id":old}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
    let mut incompatible = current;
    incompatible["function"]["parameters"]["properties"]["path"]["type"] = json!("integer");
    assert!(validate_history(&history, &[incompatible], &registry).is_err());
}

#[test]
fn data_interpretation_additions_preserve_prior_table_history() {
    let tools = crate::tools::knowledge::data::tools(&crate::config::types::Config::default());
    let current = tools[0].schema();
    let mut before = current.clone();
    let properties = before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap();
    for property in [
        "row_ids",
        "brush",
        "calculation_reference",
        "bins",
        "order_by",
        "whole_source",
        "json_pointers",
        "decimal_columns",
        "conversions",
        "dates",
        "dictionary",
        "join",
    ] {
        properties.remove(property);
    }
    properties.get_mut("format").unwrap()["enum"] = json!(["json", "csv", "xlsx"]);
    properties.get_mut("action").unwrap()["enum"] =
        json!(["profile", "select", "aggregate", "chart"]);
    let mut registry = Registry::new();
    registry.register(std::sync::Arc::clone(&tools[0]));
    let old = identities(&[before], &registry)["data"].clone();
    let history = vec![json!({"role":"assistant", "dial_tool_contracts":{"id":old}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
    let mut incompatible = current;
    incompatible["function"]["parameters"]["properties"]["path"]["type"] = json!("integer");
    assert!(validate_history(&history, &[incompatible], &registry).is_err());
}

#[test]
fn session_search_layer_views_preserve_pre_overlay_contracts() {
    let registry = Registry::new();
    for before in [
        crate::storage::session::session_search_tool::schema(),
        crate::storage::session::session_search_tool::schema_owned(),
    ] {
        let mut current = before.clone();
        crate::storage::snapcompact::schema(&mut current["function"]["parameters"]);
        let old = identities(&[before], &registry)["session_search"].clone();
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old}})];
        validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
        assert!(
            project_history(&history, std::slice::from_ref(&current), &registry)
                .unwrap()
                .is_none(),
            "additive views must not rewrite settled history"
        );
        current["function"]["parameters"]["properties"]["query"]["type"] = json!("integer");
        assert!(
            validate_history(&history, &[current], &registry).is_err(),
            "unrelated contract changes must still be refused"
        );
    }
}

#[test]
fn task_acceptance_contract_is_additive_at_both_call_shapes() {
    let registry = Registry::new();
    let mut before = task_schema(
        &[
            "run", "start", "resume", "status", "wait", "cancel", "create", "push", "peek", "close",
        ],
        true,
    );
    before["function"]["parameters"]["properties"]["tasks"] = json!({"type":"array",
        "items":{"type":"object","properties":{"task":{"type":"string"}}}});
    let old = identities(&[before.clone()], &registry)["task"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"call":old}})];
    let mut current = before;
    let properties = &mut current["function"]["parameters"]["properties"];
    let checks =
        json!({"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1}});
    properties["acceptance_checks"] = checks.clone();
    properties["tasks"]["items"]["properties"]["acceptance_checks"] = checks;
    assert!(
        project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none()
    );
    current["function"]["parameters"]["properties"]["tasks"]["items"]["properties"]["task"]["type"] =
        json!("integer");
    assert!(project_history(&history, &[current], &registry).is_err());
}

#[test]
fn git_reflog_preserves_existing_session_contracts() {
    let mut registry = Registry::new();
    let tool = crate::tools::vcs::git::tools(&crate::config::types::Config::default())
        .pop()
        .unwrap();
    let current = tool.schema();
    registry.register(tool);
    let mut before = current.clone();
    let actions = before["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array_mut()
        .unwrap();
    let count = actions.len();
    actions.retain(|value| value != "reflog");
    assert_eq!(actions.len() + 1, count);
    let identity = identities(&[before], &registry)["git"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"call":identity}})];
    assert!(
        project_history(&history, &[current], &registry)
            .unwrap()
            .is_none()
    );
}

#[test]
fn registry_contracts_accept_each_intermediate_additive_release_shape() {
    let config = crate::config::types::Config::default();
    for tool in crate::tools::net::browser::tools(&config)
        .into_iter()
        .chain(crate::tools::exec::eval::tools(&config))
        .chain(crate::tools::knowledge::data::tools(&config))
        .chain(crate::tools::exec::background::tools(&config))
        .chain(crate::tools::code::lsp::tools(&config))
        .chain(crate::tools::code::dap::tools(&BTreeMap::new()))
    {
        let name = tool.name().to_owned();
        let owner = if name.starts_with("dap_") {
            "dap"
        } else {
            &name
        };
        let Some((_, properties)) = ADDITIVE_PROPERTIES.iter().find(|(tool, _)| *tool == owner)
        else {
            continue;
        };
        let current = tool.schema();
        let mut registry = Registry::new();
        registry.register(tool);
        let mut before = current.clone();
        for remaining in (0..=properties.len()).rev() {
            if remaining < properties.len() {
                before["function"]["parameters"]["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove(properties[remaining]);
            }
            let mut narrowed = before.clone();
            let groups: Vec<_> = ADDITIVE_ENUM_VALUES
                .iter()
                .filter(|(tool, ..)| *tool == owner)
                .collect();
            for index in (0..=groups.len()).rev() {
                if index < groups.len() {
                    let (_, property, values) = groups[index];
                    if let Some(options) = narrowed["function"]["parameters"]["properties"]
                        [*property]["enum"]
                        .as_array_mut()
                    {
                        options.retain(|value| !values.contains(&value.as_str().unwrap()));
                    }
                }
                let old = identities(std::slice::from_ref(&narrowed), &registry)[&name].clone();
                let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old}})];
                validate_history(&history, std::slice::from_ref(&current), &registry)
                    .unwrap_or_else(|error| {
                        panic!("{name} properties={remaining}, enum_groups={index}: {error}")
                    });
            }
        }
        let mut incompatible = current.clone();
        incompatible["function"]["strict"] = json!(true);
        let old = identities(&[incompatible], &registry)[&name].clone();
        assert!(
            validate_history(
                &[json!({"role":"assistant","dial_tool_contracts":{"id":old}})],
                &[current],
                &registry
            )
            .is_err()
        );
    }
}

#[test]
fn contract_identity_is_independent_of_nested_object_insertion_order() {
    let first: Value = serde_json::from_str(r#"{"function":{"name":"test","parameters":{"properties":{"z":{"const":{"b":2,"a":1}},"a":{"type":"string"}},"type":"object"}}}"#).unwrap();
    let second: Value = serde_json::from_str(r#"{ "function": { "parameters": { "type": "object", "properties": { "a": {"type": "string"}, "z": {"const": {"a": 1, "b": 2}} } }, "name": "test" } }"#).unwrap();
    assert_eq!(
        identities(&[first], &Registry::new()),
        identities(&[second], &Registry::new())
    );
}

fn old_names_history(tool: &str, before: Value, arguments: &str) -> Vec<Value> {
    let old = identities(std::slice::from_ref(&before), &Registry::new())[tool].clone();
    vec![
        json!({"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":tool,"arguments":arguments}}],"dial_tool_contracts":{"c":old}}),
        json!({"role":"tool","tool_call_id":"c","content":"recorded"}),
    ]
}

#[test]
fn history_recorded_under_pre_rename_parameter_names_projects_onto_the_simpler_names() {
    let registry = Registry::new();
    let mode = crate::config::types::EditMode::Replace;
    // grep: fixed_string -> literal, max_results -> limit.
    let current = crate::tools::schema("grep", mode).unwrap();
    let mut before = current.clone();
    legacy_schema(&mut before);
    assert!(
        before
            .pointer("/function/parameters/properties/fixed_string")
            .is_some()
    );
    let history = old_names_history(
        "grep",
        before,
        r#"{"pattern":"x","fixed_string":true,"max_results":5}"#,
    );
    let projected = project_history(&history, std::slice::from_ref(&current), &registry)
        .unwrap()
        .expect("old grep names are projected");
    let arguments: Value = serde_json::from_str(
        projected[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(arguments, json!({"pattern":"x","literal":true,"limit":5}));
    validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();
    assert!(
        project_history(&projected, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none()
    );

    // grep/glob: `skip` plus a structured cursor became one string cursor.
    for (tool, recorded, expected) in [
        (
            "grep",
            r#"{"pattern":"x","limit":5,"cursor":{"path":"a.rs","match":3},"skip":2}"#,
            json!({"pattern":"x","limit":5,"cursor":"3:2:a.rs"}),
        ),
        (
            "glob",
            r#"{"pattern":"*.rs","skip":7}"#,
            json!({"pattern":"*.rs","cursor":"7"}),
        ),
        (
            "glob",
            r#"{"pattern":"*.rs","skip":0}"#,
            json!({"pattern":"*.rs"}),
        ),
    ] {
        let current = crate::tools::schema(tool, mode).unwrap();
        let mut before = current.clone();
        paged_cursor_schema(&mut before);
        assert_ne!(
            before, current,
            "{tool} has a string cursor to restore from"
        );
        let history = old_names_history(tool, before, recorded);
        let projected = project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .unwrap_or_else(|| panic!("old {tool} paging is projected"));
        let arguments: Value = serde_json::from_str(
            projected[0]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(arguments, expected, "{tool}");
        validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();
    }

    // bash: the removed async alias becomes background.
    let current = crate::tools::schema("bash", mode).unwrap();
    let mut before = current.clone();
    legacy_schema(&mut before);
    let history = old_names_history("bash", before, r#"{"command":"sleep 1","async":true}"#);
    let projected = project_history(&history, std::slice::from_ref(&current), &registry)
        .unwrap()
        .expect("old bash alias is projected");
    let arguments: Value = serde_json::from_str(
        projected[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(arguments, json!({"command":"sleep 1","background":true}));

    // find (registry tool): grep_keywords -> keywords.
    let current = crate::tools::code::navigation::tools(&crate::config::types::Config::default())
        .into_iter()
        .find(|tool| tool.name() == "find")
        .unwrap()
        .schema();
    let mut before = current.clone();
    legacy_schema(&mut before);
    let history = old_names_history("find", before, r#"{"query":"q","grep_keywords":["a"]}"#);
    let projected = project_history(&history, std::slice::from_ref(&current), &registry)
        .unwrap()
        .expect("old find names are projected");
    assert!(
        projected[0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("\"keywords\"")
    );

    // Any other change is still refused.
    let mut changed = crate::tools::schema("grep", mode).unwrap();
    changed["function"]["parameters"]["properties"]["extra"] = json!({"type":"string"});
    let history = old_names_history("grep", changed, "{}");
    assert!(
        project_history(
            &history,
            &[crate::tools::schema("grep", mode).unwrap()],
            &registry
        )
        .is_err()
    );
}

#[test]
fn optional_tool_bundles_preserve_prior_history_and_validate_shapes() {
    let config = crate::config::types::Config::default();
    let mut schemas = vec![
        crate::tools::schema("write", config.edit.mode).unwrap(),
        crate::tools::schema("grep", config.edit.mode).unwrap(),
    ];
    schemas.extend(
        crate::tools::net::web::tools(&config)
            .into_iter()
            .filter(|tool| tool.name() == "web_search")
            .map(|tool| tool.schema()),
    );
    schemas.extend(
        crate::tools::net::browser::tools(&config)
            .into_iter()
            .map(|tool| tool.schema()),
    );
    let registry = Registry::new();
    for current in schemas {
        let name = current["function"]["name"].as_str().unwrap();
        let property = match name {
            "write" => "verify",
            "grep" => "preview",
            "web_search" => "fetch_top",
            "browser" => "observe",
            _ => panic!("unexpected schema {name}"),
        };
        let mut before = current.clone();
        assert!(
            before["function"]["parameters"]["properties"]
                .as_object_mut()
                .unwrap()
                .remove(property)
                .is_some(),
            "{name}.{property}"
        );
        let old = identities(&[before], &registry)[name].clone();
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{name:old}})];
        validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
        let mut changed = current.clone();
        changed["function"]["parameters"]["properties"][property]["type"] = json!("null");
        let malformed = identities(&[changed], &registry)[name].clone();
        let history = vec![json!({"role":"assistant","dial_tool_contracts":{name:malformed}})];
        assert!(
            validate_history(&history, std::slice::from_ref(&current), &registry).is_err(),
            "changed {name}.{property} is not additive"
        );
    }
}

/// A minimal current `task` schema with the pool and `verify` actions and the profile list.
fn profiled_task_schema(profiles: &[&str]) -> Value {
    json!({"type":"function","function":{"name":"task","parameters":{"type":"object","additionalProperties":false,"properties":{
        "action":{"type":"string","enum":["run","start","resume","status","wait","cancel","create","push","peek","close","verify"]},
        "concurrency":{"type":"integer"},
        "contract":{"type":"object"},
        "model_profile":{"type":"string","enum":profiles},
        "tasks":{"type":"array","items":{"type":"object","properties":{"task":{"type":"string"}}}}
    }}}})
}

#[test]
fn task_identity_ignores_the_profile_list_and_accepts_history_hashed_with_it() {
    let registry = Registry::new();
    let current = profiled_task_schema(&["deep", "fast"]);
    // New identities no longer depend on which profiles are configured.
    assert_eq!(
        identities(&[profiled_task_schema(&["fast"])], &registry)["task"],
        identities(std::slice::from_ref(&current), &registry)["task"]
    );
    let recorded = |hash: String| {
        vec![
            json!({"role":"assistant","dial_tool_contracts":{"c":{"name":"task","revision":1,"schema_sha256":hash}}}),
        ]
    };
    // Older history hashed the list in (as the current list), or predates `verify`/`contract`.
    let mut old = profiled_task_schema(&["deep", "fast"]);
    validate_history(
        &recorded(shape_sha256(&old)),
        std::slice::from_ref(&current),
        &registry,
    )
    .unwrap();
    old["function"]["parameters"]["properties"]["action"]["enum"] = json!([
        "run", "start", "resume", "status", "wait", "cancel", "create", "push", "peek", "close"
    ]);
    old["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("contract");
    validate_history(
        &recorded(shape_sha256(&old)),
        std::slice::from_ref(&current),
        &registry,
    )
    .unwrap();
    // A changed type is not an additive difference.
    old["function"]["parameters"]["properties"]["concurrency"]["type"] = json!("string");
    assert!(
        validate_history(
            &recorded(shape_sha256(&old)),
            std::slice::from_ref(&current),
            &registry
        )
        .is_err()
    );
}

// Controller schema stubs avoid a dependency on the higher-layer dial-agent crate.
fn completion_controller_schema(name: &str) -> Value {
    let properties = match name {
        "todo" => json!({"tasks":{"type":"array","items":{
            "type":"object","additionalProperties":false,
            "properties":{"text":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},
            "required":["text","status"]
        }}}),
        "finish" => {
            json!({"outcome":{"type":"string","enum":["completed","blocked","needs_input"]},"summary":{"type":"string"}})
        }
        _ => unreachable!(),
    };
    json!({"type":"function","function":{"name":name,"parameters":{
        "type":"object","additionalProperties":false,"properties":properties
    }}})
}

#[test]
fn todo_kind_and_finish_answered_preserve_calls_and_callback_catalogs_without_rewriting() {
    let registry = Registry::new();
    for name in ["todo", "finish"] {
        let before = completion_controller_schema(name);
        let mut current = before.clone();
        if name == "todo" {
            current["function"]["parameters"]["properties"]["tasks"]["items"]["properties"]["kind"] = json!({"type":"string","enum":["step","follow_up"],"description":"Optional; defaults to step."});
        } else {
            current["function"]["parameters"]["properties"]["outcome"]["enum"] =
                json!(["completed", "answered", "blocked", "needs_input"]);
        }
        let old = identities(std::slice::from_ref(&before), &registry)[name].clone();
        let histories = [
            old_names_history(name, before, "{}"),
            vec![
                json!({"role":"assistant","dial_tool_contracts":{format!("callback:{name}"):old}}),
            ],
        ];
        for history in histories {
            let original = history.clone();
            assert!(
                project_history(&history, std::slice::from_ref(&current), &registry)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(history, original);
        }
    }
}

#[test]
fn completion_controller_additions_reject_other_contract_changes() {
    let registry = Registry::new();
    for name in ["todo", "finish"] {
        let before = completion_controller_schema(name);
        let mut current = before.clone();
        let addition = if name == "todo" {
            "/function/parameters/properties/tasks/items/properties/kind"
        } else {
            "/function/parameters/properties/outcome"
        };
        if name == "todo" {
            current["function"]["parameters"]["properties"]["tasks"]["items"]["properties"]["kind"] =
                json!({"type":"string","enum":["step","follow_up"]});
        } else {
            current["function"]["parameters"]["properties"]["outcome"]["enum"] =
                json!(["completed", "answered", "blocked", "needs_input"]);
        }
        let history = old_names_history(name, before, "{}");
        let changes = [
            (
                "/function/parameters/additionalProperties".to_owned(),
                json!(true),
            ),
            (format!("{addition}/type"), json!("integer")),
            (
                format!("{addition}/enum"),
                json!(["completed", "answered", "blocked", "needs_input", "unknown"]),
            ),
            (
                "/function/parameters/properties/extra".to_owned(),
                json!({"type":"boolean"}),
            ),
            (
                if name == "todo" {
                    "/function/parameters/properties/tasks/items/required"
                } else {
                    "/function/parameters/required"
                }
                .to_owned(),
                if name == "todo" {
                    json!(["text", "status", "kind"])
                } else {
                    json!(["outcome"])
                },
            ),
        ];
        for (pointer, value) in changes {
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            let mut changed = current.clone();
            changed.pointer_mut(parent).unwrap()[key] = value;
            assert!(
                validate_history(&history, &[changed], &registry).is_err(),
                "{name}: {pointer}"
            );
        }
        let mut strict = current.clone();
        strict["function"]["strict"] = json!(true);
        assert!(validate_history(&history, &[strict], &registry).is_err());
        let mut unknown_revision = history.clone();
        unknown_revision[0]["dial_tool_contracts"]["c"]["revision"] = json!(2);
        assert!(validate_history(&unknown_revision, &[current], &registry).is_err());
    }
}

fn evidence_registry() -> (Registry, Value) {
    let mut registry = Registry::new();
    for tool in crate::tools::knowledge::evidence::tools(&crate::config::types::Config::default()) {
        registry.register(tool);
    }
    let current = registry.get("evidence").unwrap().schema();
    (registry, current)
}

/// The evidence family merged into `evidence`; each recorded call resumes as the `evidence`
/// action that replaced it, with its other arguments unchanged.
#[test]
fn recorded_evidence_family_calls_project_onto_evidence_actions() {
    use crate::tools::knowledge::{investigate, reality, retrieval, timeline, workbench};
    let (registry, current) = evidence_registry();
    let reference = format!("artifact://{}", "a".repeat(64));
    for (schema, arguments, action) in [
        (
            retrieval::schema(),
            json!({"query":"timeout","references":[reference]}),
            "search",
        ),
        (
            workbench::schema(),
            json!({"action":"inspect","references":[reference]}),
            "audit",
        ),
        (
            investigate::schema(),
            json!({"action":"compare","reference":reference,"other":reference}),
            "diff",
        ),
        (
            reality::schema(),
            json!({"action":"assess","expectation":reference,"observation":reference,"expectation_quote":"q","claim":"c","assessment":"mismatch"}),
            "assess",
        ),
        (
            timeline::schema(),
            json!({"action":"reconstruct"}),
            "timeline",
        ),
        (
            crate::tools::knowledge::perception::schema(),
            json!({"action":"close","reason":"done"}),
            "budget_close",
        ),
    ] {
        let name = schema["function"]["name"].as_str().unwrap().to_owned();
        let old = identities(&[schema], &Registry::new())[&name].clone();
        let history = vec![
            json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":name,"arguments":arguments.to_string()}}],"dial_tool_contracts":{"c1":old}}),
            json!({"role":"tool","tool_call_id":"c1","content":"{}"}),
        ];
        let projected = project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .unwrap_or_else(|| panic!("{name} must project onto evidence"));
        let call = &projected[0]["tool_calls"][0]["function"];
        assert_eq!(call["name"], "evidence", "{name}");
        let projected_arguments: Value =
            serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        let mut expected = arguments.clone();
        expected["action"] = json!(action);
        assert_eq!(projected_arguments, expected, "{name}");
        crate::tools::surface::catalog::validate_arguments(&current, &projected_arguments)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();
        assert!(
            project_history(&projected, std::slice::from_ref(&current), &registry)
                .unwrap()
                .is_none(),
            "{name}: projection is idempotent"
        );

        // An action the retired tool never had is not a shape it recorded.
        let mut unknown = history.clone();
        unknown[0]["tool_calls"][0]["function"]["arguments"] =
            json!(json!({"action":"capture"}).to_string());
        assert!(
            project_history(&unknown, std::slice::from_ref(&current), &registry).is_err(),
            "{name}"
        );
    }
}

/// Sessions recorded under the pre-merge `evidence` contract still resume.
#[test]
fn recorded_pre_merge_evidence_contract_is_accepted() {
    let (registry, current) = evidence_registry();
    let mut before = current.clone();
    let native = [
        "action",
        "reference",
        "other",
        "path",
        "start",
        "count",
        "retain_source",
        "region",
        "summary",
        "parents",
        "clock",
        "alignment",
        "time_ms",
    ];
    before["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .retain(|name, _| native.contains(&name.as_str()));
    before["function"]["parameters"]["properties"]["action"]["enum"] = json!([
        "capture",
        "snapshot",
        "inspect",
        "context",
        "crop",
        "compare",
        "freshness",
        "derive",
        "align"
    ]);
    let old = identities(&[before], &registry)["evidence"].clone();
    assert_ne!(
        old,
        identities(std::slice::from_ref(&current), &registry)["evidence"]
    );
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"id":old}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
}

/// `todo` with and without the state ops that replaced `working_state`.
fn todo_schemas() -> (Value, Value) {
    let before = json!({"type":"function","function":{"name":"todo","parameters":{"type":"object","properties":{
        "op":{"type":"string","enum":["set","append","start","done","drop","block","view"]},
        "tasks":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"kind":{"type":"string","enum":["step","follow_up"]}}}}
    }}}});
    let mut current = before.clone();
    let properties = current["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap();
    properties["op"]["enum"] = json!([
        "set",
        "append",
        "start",
        "done",
        "drop",
        "block",
        "view",
        "state_view",
        "state_update",
        "state_patch"
    ]);
    properties.insert(
        "expected_revision".into(),
        json!({"type":"integer","minimum":0}),
    );
    properties.insert("state".into(), json!({"type":"object"}));
    (before, current)
}

fn working_state_stub() -> Value {
    json!({"type":"function","function":{"name":"working_state","parameters":{"type":"object","properties":{
        "action":{"type":"string","enum":["view","update","patch"]},
        "expected_revision":{"type":"integer","minimum":0},
        "goal":{"type":"string"}
    },"required":["action"],"additionalProperties":false}}})
}

#[test]
fn todo_recorded_before_state_ops_resumes_but_other_changes_are_refused() {
    let registry = Registry::new();
    let (before, current) = todo_schemas();
    let history = old_names_history("todo", before, r#"{"op":"view"}"#);
    assert!(
        project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none()
    );
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
    let mut changed = current.clone();
    changed["function"]["parameters"]["properties"]["state"] = json!({"type":"string"});
    let mut extra_op = current.clone();
    extra_op["function"]["parameters"]["properties"]["op"]["enum"]
        .as_array_mut()
        .unwrap()
        .push(json!("state_drop"));
    for changed in [changed, extra_op] {
        assert!(validate_history(&history, &[changed], &registry).is_err());
    }
}

#[test]
fn recorded_working_state_calls_project_onto_todo_state_ops() {
    let registry = Registry::new();
    let (_, current) = todo_schemas();
    for (arguments, expected) in [
        (json!({"action":"view"}), json!({"op":"state_view"})),
        (
            json!({"action":"update","goal":"ship","deliverables":[{"id":"d","description":"x","status":"open"}]}),
            json!({"op":"state_update","state":{"goal":"ship","deliverables":[{"id":"d","description":"x","status":"open"}]}}),
        ),
        (
            json!({"action":"patch","expected_revision":3,"goal":"ship"}),
            json!({"op":"state_patch","expected_revision":3,"state":{"goal":"ship"}}),
        ),
    ] {
        let history = old_names_history(
            "working_state",
            working_state_stub(),
            &arguments.to_string(),
        );
        let projected = project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .expect("working_state history projects onto todo");
        let call = &projected[0]["tool_calls"][0]["function"];
        assert_eq!(call["name"], "todo");
        let projected_arguments: Value =
            serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(projected_arguments, expected);
        assert_eq!(working_state_as_todo(&arguments), Some(expected));
        validate_history(&projected, std::slice::from_ref(&current), &registry).unwrap();
        assert!(
            project_history(&projected, std::slice::from_ref(&current), &registry)
                .unwrap()
                .is_none(),
            "projection is idempotent"
        );
    }
    let unknown = old_names_history(
        "working_state",
        working_state_stub(),
        r#"{"action":"drop"}"#,
    );
    assert!(project_history(&unknown, std::slice::from_ref(&current), &registry).is_err());
}

#[test]
fn merged_history_stays_inert_while_its_successor_is_not_callable() {
    let registry = Registry::new();
    let unrelated = schema("string");
    for history in [
        old_names_history(
            "working_state",
            working_state_stub(),
            r#"{"action":"view"}"#,
        ),
        old_names_history(
            "workbench",
            crate::tools::knowledge::workbench::schema(),
            r#"{"action":"inspect","references":["artifact://x"]}"#,
        ),
    ] {
        let original = history.clone();
        assert!(
            project_history(&history, std::slice::from_ref(&unrelated), &registry)
                .unwrap()
                .is_none()
        );
        validate_history(&history, std::slice::from_ref(&unrelated), &registry).unwrap();
        assert_eq!(history, original);
    }
}

/// `monitor` is exposed through `bg`, so the optional properties it gained after release are
/// additions to the `bg` contract too: a session recorded before them must still resume.
#[test]
fn a_consolidated_successor_accepts_history_from_before_a_members_additions() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::config::types::Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        ..crate::config::types::Config::default()
    };
    let mut registry = Registry::new();
    for tool in crate::tools::exec::background::tools(&config) {
        registry.register(tool);
    }
    let current = registry.get("bg").unwrap().schema();
    let properties = &current["function"]["parameters"]["properties"];
    assert!(properties["command"].is_object() && properties["after_secs"].is_object());
    let recorded = |removed: &[&str]| {
        let mut schema = current.clone();
        let properties = schema["function"]["parameters"]["properties"]
            .as_object_mut()
            .unwrap();
        for property in removed {
            assert!(properties.remove(*property).is_some(), "{property}");
        }
        let old = identities(&[schema], &registry)["bg"].clone();
        vec![
            json!({"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"bg","arguments":"{\"action\":\"list\"}"}}],"dial_tool_contracts":{"c1":old}}),
            json!({"role":"tool","tool_call_id":"c1","content":"no jobs"}),
        ]
    };
    let schemas = std::slice::from_ref(&current);
    // The shape a 0.3.4 session recorded, each later release, and the members' additions alone.
    for removed in [
        &["after_secs", "on_exit", "note", "pid", "save"][..],
        &["note", "pid", "save"],
        &["run", "after_secs", "on_exit", "note", "pid", "save"],
        &["call", "after_secs", "on_exit", "note", "pid", "save"],
        &["persist", "lifetime", "call", "pid", "save"],
        &["pid", "save"],
    ] {
        let projected = project_history(&recorded(removed), schemas, &registry);
        assert!(matches!(projected, Ok(None)), "{removed:?}: {projected:?}");
    }
    // Anything that is not a listed addition is still a contract change.
    for removed in [&["pattern"][..], &["command"], &["after_secs", "job"]] {
        assert!(
            project_history(&recorded(removed), schemas, &registry).is_err(),
            "{removed:?}"
        );
    }
}

#[test]
fn write_artifact_batch_contract_accepts_the_exact_legacy_required_shape() {
    let registry = Registry::new();
    let current = crate::tools::schema("write", crate::config::types::EditMode::default()).unwrap();
    let mut old = current.clone();
    for property in [
        "source",
        "files",
        "if_absent",
        "expected_sha256",
        "idempotent",
        "receipt",
    ] {
        old["function"]["parameters"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove(property);
    }
    old["function"]["parameters"]["required"] = json!(["path", "content"]);
    let identity = identities(&[old.clone()], &registry)["write"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"call":identity}})];
    validate_history(&history, std::slice::from_ref(&current), &registry).unwrap();
    old["function"]["parameters"]["properties"]["content"]["type"] = json!("number");
    let identity = identities(&[old], &registry)["write"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"call":identity}})];
    assert!(validate_history(&history, &[current], &registry).is_err());
}

#[test]
fn codebase_navigate_action_resumes_pre_navigate_memory_contracts() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(project.path(), home.path().join("dial"))
            .unwrap();
    let cache =
        std::sync::Arc::new(crate::tools::knowledge::codebase::Codebase::open(&layout).unwrap());
    let tool = crate::tools::knowledge::codebase::tool(cache, "session".into());
    let current = tool.schema();
    let mut before = current.clone();
    let actions = before["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array_mut()
        .unwrap();
    let count = actions.len();
    actions.retain(|value| value != "navigate");
    assert_eq!(actions.len() + 1, count);
    let mut registry = Registry::new();
    registry.register(tool);
    let identity = identities(&[before], &registry)["codebase_memory"].clone();
    let history = vec![json!({"role":"assistant","dial_tool_contracts":{"call":identity}})];
    assert!(
        project_history(&history, &[current], &registry)
            .unwrap()
            .is_none()
    );
}

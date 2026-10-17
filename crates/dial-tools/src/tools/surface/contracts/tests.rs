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

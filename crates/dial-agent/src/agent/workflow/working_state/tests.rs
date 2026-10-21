use super::*;

#[test]
fn patch_preserves_omitted_fields_and_refuses_stale_revision() {
    let original =
        json!({"goal":"repair", "constraints":["preserve API"], "evidence":["artifact://old"]});
    let view = apply(&original, &json!({"action":"view"})).unwrap();
    assert_eq!(view["revision"], 0);
    let patched = apply(
        &original,
        &json!({"action":"patch","expected_revision":0,"open_work":["test"]}),
    )
    .unwrap();
    assert_eq!(patched["goal"], original["goal"]);
    assert_eq!(patched["constraints"], original["constraints"]);
    assert_eq!(patched["evidence"], original["evidence"]);
    assert_eq!(patched["revision"], 1);
    assert!(
        apply(
            &patched,
            &json!({"action":"patch","expected_revision":0,"goal":"oops"})
        )
        .is_err()
    );
    assert!(apply(&patched, &json!({"action":"patch","goal":"oops"})).is_err());
    assert_eq!(patched["goal"], "repair");
}

#[test]
fn legacy_update_replaces_and_old_empty_sessions_work() {
    let first = apply(
        &Value::Null,
        &json!({"action":"update","goal":"old","constraints":["old"]}),
    )
    .unwrap();
    let second = apply(&first, &json!({"action":"update","goal":"new"})).unwrap();
    assert!(second.get("constraints").is_none());
    assert_eq!(second["goal"], "new");
    assert_eq!(second["revision"], 2);
    assert_eq!(
        apply(&Value::Null, &json!({"action":"view"})).unwrap(),
        json!({"revision":0})
    );
}

#[test]
fn claims_can_be_retracted_without_promoting_source_refs() {
    let claim = json!({"id":"check-1","text":"build passed","status":"unverified","source_refs":["artifact://model-invented"]});
    let state = apply(&Value::Null, &json!({"action":"update","claims":[claim]})).unwrap();
    assert_eq!(state["claims"][0]["status"], "unverified");
    let mut retracted = state["claims"][0].clone();
    retracted["status"] = json!("retracted");
    let state = apply(
        &state,
        &json!({"action":"patch","expected_revision":1,"claims":[retracted]}),
    )
    .unwrap();
    assert_eq!(state["claims"][0]["id"], "check-1");
    assert_eq!(state["claims"][0]["status"], "retracted");
}

#[test]
fn malformed_and_oversized_changes_fail_instead_of_truncating() {
    let claim = json!({"id":"c","text":"claim","status":"unverified","source_refs":[]});
    for args in [
        json!({"action":"update","claims":[claim.clone(),claim.clone()]}),
        json!({"action":"update","claims":vec![claim.clone();65]}),
        json!({"action":"patch","expected_revision":-1}),
        json!({"action":"bogus"}),
        json!({"action":"update","goal":3}),
        json!({"action":"update","unknown":"x"}),
        json!({"action":"update","goal":"x".repeat(MAX_STATE_BYTES)}),
        json!({"action":"update","claims":[{"id":"c","text":"x","status":"verified","source_refs":[]}]}),
        json!({"action":"update","claims":[{"id":"c","text":"é".repeat(4096),"status":"unverified","source_refs":[]}]}),
        json!({"action":"update","claims":[{"id":"c","text":"x","status":"unverified","source_refs":vec!["ref";17]}]}),
    ] {
        assert!(
            apply(&json!({"goal":"unchanged"}), &args).is_err(),
            "accepted {args}"
        );
    }
}

#[test]
fn historical_replace_contract_is_compatible_but_other_changes_are_not() {
    use crate::tools::{integrations::ext::Registry, surface::contracts};
    let registry = Registry::new();
    let current = schema();
    let mut old = current.clone();
    let properties = old["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap();
    properties.remove("expected_revision");
    properties.remove("claims");
    properties["action"]["enum"] = json!(["view", "update"]);
    let identity = contracts::identities(&[old.clone()], &registry)["working_state"].clone();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"w","type":"function","function":{"name":"working_state","arguments":"{\"action\":\"update\",\"goal\":\"old\"}"}}],"dial_tool_contracts":{"w":identity}}),
        json!({"role":"tool","tool_call_id":"w","content":"historical result"}),
    ];
    assert!(
        contracts::project_history(&history, &[current.clone()], &registry)
            .unwrap()
            .is_none()
    );
    let mut incompatible = current;
    incompatible["function"]["parameters"]["required"] = json!(["action", "goal"]);
    assert!(contracts::project_history(&history, &[incompatible], &registry).is_err());
}

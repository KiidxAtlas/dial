use super::*;

fn deliverables(count: usize) -> Vec<Value> {
    (0..count)
        .map(|index| json!({"id":format!("d{index}"),"description":"x","status":"open"}))
        .collect()
}

fn deliverable(id: &str, status: &str) -> Value {
    json!({"id":id,"description":"deliverable","status":status})
}

fn confirm(id: &str, action: Action) -> Confirmation {
    Confirmation {
        id: id.into(),
        action,
        question_id: "q-1".into(),
    }
}

fn change(current: &Value, args: Value, confirmations: &[Confirmation]) -> Change {
    prepare(current, &args, Some("turn-1"), confirmations).unwrap()
}

fn with_deliverables(items: Vec<Value>) -> Value {
    apply(
        &Value::Null,
        &json!({"action":"update","deliverables":items}),
    )
    .unwrap()
}

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
fn deliverable_count_and_evidence_limits_are_inclusive_and_exceeded_writes_are_refused() {
    assert!(
        apply(
            &Value::Null,
            &json!({"action":"update","deliverables":deliverables(32)})
        )
        .is_ok()
    );
    assert!(
        apply(
            &Value::Null,
            &json!({"action":"update","deliverables":deliverables(33)})
        )
        .is_err()
    );
    let max_refs: Vec<String> = (0..16)
        .map(|index| format!("artifact://ref-{index}"))
        .collect();
    let satisfied =
        json!({"id":"d","description":"x","status":"satisfied","evidence_refs":max_refs});
    assert!(
        apply(
            &Value::Null,
            &json!({"action":"update","deliverables":[satisfied]})
        )
        .is_ok()
    );
}

#[test]
fn malformed_deliverable_changes_fail_atomically() {
    let cases = [
        json!({"action":"update","deliverables":[deliverable("d","open"),deliverable("d","open")]}),
        json!({"action":"update","deliverables":[{"id":"","description":"x","status":"open"}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"  ","status":"open"}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"x".repeat(1025),"status":"open"}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"x","status":"done"}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"x","status":"satisfied"}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"x","status":"satisfied","evidence_refs":[]}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"x","status":"open","evidence_refs":vec!["a";17]}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"x","status":"open","evidence_refs":[""]}]}),
        json!({"action":"update","deliverables":"not-an-array"}),
    ];
    for args in cases {
        assert!(
            apply(&json!({"goal":"unchanged"}), &args).is_err(),
            "accepted {args}"
        );
    }
}

#[test]
fn model_cannot_write_runtime_owned_fields() {
    let forged = [
        json!({"action":"update","deliverables":[{"id":"d","description":"x","status":"open","origin":{"entry_id":"forged","trust":"user"}}]}),
        json!({"action":"update","deliverables":[{"id":"d","description":"x","status":"cancelled","receipt":{"action":"cancel","question_id":"q"}}]}),
        json!({"action":"update","last_scope_change":{"question_id":"q","changes":[]}}),
    ];
    for args in forged {
        assert!(apply(&Value::Null, &args).is_err(), "accepted {args}");
    }
}

#[test]
fn only_satisfied_or_confirmed_cancelled_deliverables_clear_completion() {
    for status in ["open", "in_progress", "blocked"] {
        let state = with_deliverables(vec![deliverable("ship", status)]);
        let blocker = completion_blocker(&state).expect("open obligation must block");
        assert!(
            blocker.contains("ship") && blocker.contains(status),
            "{blocker}"
        );
        assert_eq!(open_deliverables(&state).len(), 1);
    }
    let evidenced = with_deliverables(vec![
        json!({"id":"ship","description":"x","status":"satisfied","evidence_refs":["artifact://test-log"]}),
    ]);
    assert!(completion_blocker(&evidenced).is_none());
}

#[test]
fn cancellation_needs_confirmation_and_then_clears_completion() {
    let state = with_deliverables(vec![deliverable("ship", "open")]);
    let args = json!({"action":"update","deliverables":[deliverable("ship", "cancelled")]});
    let pending = change(&state, args.clone(), &[]);
    assert!(pending.needs_confirmation());
    assert!(pending.summary().starts_with("cancel ship"));
    let confirmed = change(&state, args, &[confirm("ship", Action::Cancel)]);
    assert!(!confirmed.needs_confirmation());
    assert!(completion_blocker(&confirmed.next).is_none());
    assert!(open_deliverables(&confirmed.next).is_empty());
    assert_eq!(
        confirmed.next["deliverables"][0]["receipt"],
        json!({"action":"cancel","question_id":"q-1"})
    );
}

#[test]
fn legacy_or_unconfirmed_cancellation_blocks_completion() {
    let cases = [
        json!(null),
        json!({"action":"redefine","question_id":"q"}),
        json!({"action":"cancel","question_id":""}),
    ];
    for receipt in cases {
        let state = json!({"deliverables":[
            {"id":"d","description":"x","status":"cancelled","receipt":receipt}
        ]});
        assert!(
            completion_blocker(&state)
                .expect("unconfirmed cancellation must block")
                .contains("cancelled without user confirmation")
        );
        assert_eq!(open_deliverables(&state).len(), 1);
    }
    let state = json!({"deliverables":[
        {"id":"d","description":"x","status":"cancelled","receipt":{"action":"cancel","question_id":"q"}}
    ]});
    assert!(completion_blocker(&state).is_none());
    assert!(open_deliverables(&state).is_empty());
}

#[test]
fn wrong_or_empty_confirmations_do_not_apply_scope_changes() {
    let state = with_deliverables(vec![deliverable("ship", "open")]);
    let args = json!({"action":"update","deliverables":[deliverable("ship", "cancelled")]});
    let empty_question = Confirmation {
        question_id: String::new(),
        ..confirm("ship", Action::Cancel)
    };
    for confirmations in [
        vec![confirm("ship", Action::Remove)],
        vec![confirm("other", Action::Cancel)],
        vec![empty_question],
    ] {
        assert!(change(&state, args.clone(), &confirmations).needs_confirmation());
    }
}

#[test]
fn omitted_deliverables_are_preserved_on_update_and_patch() {
    let state = with_deliverables(vec![deliverable("a", "open"), deliverable("b", "open")]);
    let updated = change(&state, json!({"action":"update","goal":"new"}), &[]);
    assert!(!updated.needs_confirmation());
    assert_eq!(updated.next["deliverables"], state["deliverables"]);
    let patched = apply(
        &state,
        &json!({"action":"patch","expected_revision":1,"goal":"new"}),
    )
    .unwrap();
    assert_eq!(patched["deliverables"], state["deliverables"]);
}

#[test]
fn removal_needs_confirmation_and_leaves_a_durable_receipt() {
    let state = with_deliverables(vec![deliverable("a", "open"), deliverable("b", "open")]);
    let args = json!({"action":"update","deliverables":[deliverable("a", "open")]});
    let pending = change(&state, args.clone(), &[]);
    assert!(pending.needs_confirmation());
    assert!(pending.summary().contains("remove b"));
    assert_eq!(pending.next["deliverables"].as_array().unwrap().len(), 2);
    let applied = change(&state, args, &[confirm("b", Action::Remove)]);
    assert!(!applied.needs_confirmation());
    assert_eq!(applied.next["deliverables"].as_array().unwrap().len(), 1);
    assert_eq!(applied.next["last_scope_change"]["question_id"], "q-1");
    assert_eq!(applied.next["last_scope_change"]["changes"][0]["id"], "b");
    assert_eq!(
        applied.next["last_scope_change"]["changes"][0]["action"],
        "remove"
    );
    let notes = apply(&applied.next, &json!({"action":"update","goal":"later"})).unwrap();
    assert_eq!(
        notes["last_scope_change"],
        applied.next["last_scope_change"]
    );
}

#[test]
fn redefinition_shows_old_and_new_then_resets_to_open_without_evidence() {
    let state = with_deliverables(vec![
        json!({"id":"d","description":"old text","status":"satisfied","evidence_refs":["artifact://a"]}),
    ]);
    let args = json!({"action":"update","deliverables":[
        {"id":"d","description":"new text","status":"satisfied","evidence_refs":["artifact://b"]}
    ]});
    let pending = change(&state, args.clone(), &[]);
    let summary = pending.summary();
    assert!(
        summary.contains("\"old text\"") && summary.contains("\"new text\""),
        "{summary}"
    );
    let applied = change(&state, args, &[confirm("d", Action::Redefine)]);
    let item = &applied.next["deliverables"][0];
    assert_eq!(item["description"], "new text");
    assert_eq!(item["status"], "open");
    assert!(item.get("evidence_refs").is_none());
    assert_eq!(item["receipt"]["action"], "redefine");
    assert!(completion_blocker(&applied.next).is_some());
}

#[test]
fn cancelled_is_terminal_and_cancel_with_redefinition_is_refused() {
    let state = with_deliverables(vec![deliverable("d", "open")]);
    let cancelled = change(
        &state,
        json!({"action":"update","deliverables":[deliverable("d", "cancelled")]}),
        &[confirm("d", Action::Cancel)],
    )
    .next;
    assert!(
        apply(
            &cancelled,
            &json!({"action":"update","deliverables":[deliverable("d", "open")]})
        )
        .is_err()
    );
    assert!(
        apply(
            &cancelled,
            &json!({"action":"update","deliverables":[deliverable("d", "cancelled")]})
        )
        .is_ok()
    );
    assert!(
        prepare(
            &state,
            &json!({"action":"update","deliverables":[{"id":"d","description":"new","status":"cancelled"}]}),
            None,
            &[],
        )
        .is_err()
    );
}

#[test]
fn confirmed_cancellation_retires_when_omitted_and_frees_capacity() {
    let state = with_deliverables(deliverables(32));
    let mut all = deliverables(32);
    all[0]["status"] = json!("cancelled");
    let cancelled = change(
        &state,
        json!({"action":"update","deliverables":all}),
        &[confirm("d0", Action::Cancel)],
    )
    .next;
    let mut replacement: Vec<Value> = deliverables(32).into_iter().skip(1).collect();
    replacement.push(deliverable("fresh", "open"));
    let next = change(
        &cancelled,
        json!({"action":"update","deliverables":replacement}),
        &[],
    );
    assert!(!next.needs_confirmation());
    assert_eq!(next.next["deliverables"].as_array().unwrap().len(), 32);
    assert!(
        next.next["deliverables"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["id"] != "d0")
    );
}

#[test]
fn new_deliverables_record_origin_as_model_interpretation() {
    let state = with_deliverables(vec![deliverable("a", "open")]);
    let next = change(
        &state,
        json!({"action":"update","deliverables":[deliverable("a", "open"), deliverable("b", "open")]}),
        &[],
    )
    .next;
    let b = &next["deliverables"][1];
    assert_eq!(b["origin"]["entry_id"], "turn-1");
    assert_eq!(b["origin"]["trust"], "model_interpretation");
    assert_eq!(
        next["deliverables"][0]["origin"],
        state["deliverables"][0]["origin"]
    );
}

#[test]
fn apply_refuses_pending_scope_changes_without_persisting() {
    let state = with_deliverables(vec![deliverable("a", "open")]);
    let err = apply(&state, &json!({"action":"update","deliverables":[]}))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("needs user confirmation") && err.contains("remove a"),
        "{err}"
    );
}

#[test]
fn absent_or_null_deliverables_keep_legacy_meaning() {
    for state in [json!({"goal":"x"}), json!({"goal":"x","deliverables":null})] {
        assert!(completion_blocker(&state).is_none());
        assert!(open_deliverables(&state).is_empty());
        assert!(check_references(&state).is_empty());
    }
    let legacy = apply(
        &json!({"goal":"x"}),
        &json!({"action":"patch","expected_revision":0,"open_work":["t"]}),
    )
    .unwrap();
    assert!(legacy.get("deliverables").is_none());
}

#[test]
fn malformed_stored_deliverables_block_without_panicking() {
    for stored in [
        json!({"deliverables":"oops"}),
        json!({"deliverables":["not-object", null]}),
        json!({"deliverables":[{"id":3,"description":"x","status":"open"}]}),
        json!({"deliverables":[{"id":"a","description":"x","status":"weird"}]}),
        json!({"deliverables":[{"id":"a","description":"x","status":"satisfied","evidence_refs":"bad"}]}),
        json!({"deliverables":[{"id":"a","description":"x","status":"satisfied","evidence_refs":[7]}]}),
    ] {
        assert!(completion_blocker(&stored).is_some(), "{stored}");
        let _ = open_deliverables(&stored);
        assert!(check_references(&stored).is_empty(), "{stored}");
    }
}

#[test]
fn only_satisfied_check_prefixed_refs_are_check_links() {
    let state = json!({"deliverables":[
        {"id":"build","description":"x","status":"satisfied","evidence_refs":["check:build","artifact://log","check: lint "]},
        {"id":"open","description":"x","status":"open","evidence_refs":["check:never"]}
    ]});
    assert_eq!(
        check_references(&state),
        vec![
            ("build".to_owned(), "build".to_owned()),
            ("build".to_owned(), "lint".to_owned())
        ]
    );
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
    properties.remove("deliverables");
    properties["action"]["enum"] = json!(["view", "update"]);
    let identity = contracts::identities(&[old.clone()], &registry)["working_state"].clone();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"w","type":"function","function":{"name":"working_state","arguments":"{\"action\":\"update\",\"goal\":\"old\"}"}}],"dial_tool_contracts":{"w":identity}}),
        json!({"role":"tool","tool_call_id":"w","content":"historical result"}),
    ];
    assert!(
        contracts::project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none()
    );
    let mut incompatible = current;
    incompatible["function"]["parameters"]["required"] = json!(["action", "goal"]);
    assert!(contracts::project_history(&history, &[incompatible], &registry).is_err());
}

#[test]
fn historical_contract_before_deliverables_resumes_and_later_drift_is_refused() {
    use crate::tools::{integrations::ext::Registry, surface::contracts};
    let registry = Registry::new();
    let current = schema();
    let mut revisioned = current.clone();
    revisioned["function"]["parameters"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("deliverables");
    let identity = contracts::identities(&[revisioned], &registry)["working_state"].clone();
    let history = vec![
        json!({"role":"assistant","tool_calls":[{"id":"w","type":"function","function":{"name":"working_state","arguments":"{\"action\":\"patch\",\"expected_revision\":0,\"goal\":\"old\"}"}}],"dial_tool_contracts":{"w":identity}}),
        json!({"role":"tool","tool_call_id":"w","content":"historical result"}),
    ];
    assert!(
        contracts::project_history(&history, std::slice::from_ref(&current), &registry)
            .unwrap()
            .is_none()
    );
    let mut incompatible = current;
    incompatible["function"]["parameters"]["required"] = json!(["action", "goal"]);
    assert!(contracts::project_history(&history, &[incompatible], &registry).is_err());
}

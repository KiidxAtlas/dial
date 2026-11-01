use super::*;
use crate::storage::session::Session;

#[test]
fn five_forced_session_compactions_preserve_exact_critical_state_without_promotion() {
    let mut session = Session::ephemeral();
    let control = json!({
        "current_instruction":"latest actual request\nwith exact constraints",
        "current_instruction_entry_id":"user-entry", "original_goal":"older goal",
        "working_state":{"revision":8,"constraints":["do not change APIs"],"evidence":["artifact://r"],
            "claims":[
                {"id":"open","text":"not checked","status":"unverified","source_refs":["artifact://claimed"]},
                {"id":"old","text":"disproved","status":"retracted","source_refs":[]},
                {"id":"stale","text":"older plan","status":"superseded","source_refs":[]}
            ]},
        "todo":{"tasks":[{"content":"run checks","status":"pending"}]},
        "evidence_index":["artifact://r"]
    });
    session.set_control_state(control.clone()).unwrap();
    for round in 0..5 {
        // Same real carry helper used by snapshot validation and commit installation. Summary
        // deliberately contradicts recorded claims: prose must never rewrite the contract.
        let mut prefix = vec![Session::compaction_message(&format!(
            "round {round}: everything verified"
        ))];
        prefix.push(message(&session.control_state(), &control["todo"]).unwrap());
        session
            .compact_with_prefix(format!("lossy summary {round}"), Some(prefix), 0)
            .unwrap();
        let messages = session.messages();
        let carried: Vec<_> = messages
            .iter()
            .filter(|m| m["dial_compaction_critical"] == true)
            .collect();
        assert_eq!(carried.len(), 1);
        let exact = &carried[0]["dial_critical_state"];
        assert_eq!(
            exact["runtime_user_request"]["current_instruction"],
            control["current_instruction"]
        );
        assert_eq!(
            exact["model_recorded_unverified_state"],
            control["working_state"]
        );
        assert_eq!(exact["todos"], control["todo"]);
        assert_eq!(
            session.control_state()["working_state"],
            control["working_state"]
        );
        assert!(!retained(&carried[0]));
    }
}

#[test]
fn oversized_contract_fails_explicitly_and_never_truncates_legacy_state() {
    let control = json!({"current_instruction":"é".repeat(MAX_CRITICAL_BYTES)});
    let error = message(&control, &Value::Null).unwrap_err().to_string();
    assert!(error.contains("no designated constraints or current request dropped"));
    assert_eq!(
        control["current_instruction"].as_str().unwrap().len(),
        MAX_CRITICAL_BYTES * 2
    );
}

#[test]
fn repeated_compaction_carries_user_intent_separately_from_model_working_state() {
    let mut session = Session::ephemeral();
    let control = json!({
        "current_instruction":"use v2 only",
        "current_instruction_entry_id":"steer-entry",
        "original_goal":"port the client",
        "user_intent":{
            "latest":{"text":"use v2 only","entry_id":"steer-entry","kind":"user_steering"},
            "earlier":[{"text":"port the client","entry_id":"turn-entry","kind":"user_turn"}],
            "omitted_messages":0
        },
        "working_state":{"revision":2,"claims":[{"id":"c","text":"maybe v1","status":"unverified","source_refs":[]}]}
    });
    session.set_control_state(control.clone()).unwrap();
    for round in 0..4 {
        let mut prefix = vec![Session::compaction_message(&format!("round {round}"))];
        prefix.push(message(&session.control_state(), &Value::Null).unwrap());
        session
            .compact_with_prefix(format!("summary {round}"), Some(prefix), 0)
            .unwrap();
        let carried: Vec<_> = session
            .messages()
            .into_iter()
            .filter(|m| m["dial_compaction_critical"] == true)
            .collect();
        assert_eq!(carried.len(), 1);
        let contract = &carried[0]["dial_critical_state"];
        let mut exact_intent = contract["runtime_user_request"]["user_intent"].clone();
        // Identical latest text is emitted as a JSON pointer into this same contract.
        assert_eq!(
            exact_intent["latest"]["text_ref"],
            "/runtime_user_request/current_instruction"
        );
        assert!(exact_intent["latest"]["text"].is_null());
        let resolved = contract
            .pointer("/runtime_user_request/current_instruction")
            .unwrap()
            .clone();
        exact_intent["latest"]["text"] = resolved;
        exact_intent["latest"]
            .as_object_mut()
            .unwrap()
            .remove("text_ref");
        assert_eq!(exact_intent, control["user_intent"]);
        assert_eq!(
            contract["runtime_user_request"]["current_instruction"],
            "use v2 only"
        );
        assert_eq!(contract["runtime_user_request"]["entry_id"], "steer-entry");
        assert_eq!(
            contract["model_recorded_unverified_state"],
            control["working_state"]
        );
    }
}

#[test]
fn identical_large_request_is_carried_once_and_distinct_original_and_draft_still_overflow() {
    // 40 KiB identical text would exceed the 64 KiB contract if carried twice.
    let same = "S".repeat(40 * 1024);
    let control = json!({
        "current_instruction":same,
        "current_instruction_entry_id":"big-entry",
        "user_intent":{
            "latest":{"text":same,"entry_id":"big-entry","kind":"user_turn"},
            "earlier":[],"omitted_messages":0
        }
    });
    let carried = message(&control, &Value::Null).unwrap();
    let encoded = carried["content"].as_str().unwrap();
    assert_eq!(encoded.matches(&same).count(), 1);
    assert_eq!(
        carried["dial_critical_state"]["runtime_user_request"]["user_intent"]["latest"]["text_ref"],
        "/runtime_user_request/current_instruction"
    );

    // Distinct drafted original and effective text are both kept and therefore overflow.
    let distinct = "D".repeat(40 * 1024);
    let overflow = json!({
        "current_instruction":same,
        "current_instruction_entry_id":"draft-entry",
        "user_intent":{
            "latest":{"text":distinct,"entry_id":"typed-entry","kind":"user_turn"},
            "earlier":[],"omitted_messages":0
        }
    });
    let error = message(&overflow, &Value::Null).unwrap_err().to_string();
    assert!(error.contains("no designated constraints or current request dropped"));
}

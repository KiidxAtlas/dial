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

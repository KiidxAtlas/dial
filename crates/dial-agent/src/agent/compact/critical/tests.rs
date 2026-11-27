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
fn approved_plan_is_carried_word_for_word_through_repeated_compactions() {
    let mut session = Session::ephemeral();
    let plan = crate::agent::workflow::plan_files::approved_record(
        "# Ship\n1. migrate schema\n2. backfill",
        Some(std::path::Path::new("/state/plans/s/ship.md")),
        None,
    );
    let control = json!({"current_instruction":"implement","approved_plan":plan});
    session.set_control_state(control.clone()).unwrap();
    for round in 0..3 {
        let prefix = vec![
            Session::compaction_message(&format!("round {round}: plan was roughly to migrate")),
            message(&session.control_state(), &Value::Null).unwrap(),
        ];
        session
            .compact_with_prefix(format!("lossy summary {round}"), Some(prefix), 0)
            .unwrap();
        let messages = session.messages();
        let carried = messages
            .iter()
            .find(|m| m["dial_compaction_critical"] == true)
            .unwrap();
        assert_eq!(carried["dial_critical_state"]["approved_plan"], plan);
    }
    let without = message(&json!({"current_instruction":"x"}), &Value::Null).unwrap();
    assert!(
        without["dial_critical_state"]
            .get("approved_plan")
            .is_none(),
        "no plan, no field"
    );
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

fn sha(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn carried_file(
    workspace: &crate::tools::Workspace,
    path: &str,
    fragments: &[&str],
    receipt_sha: Option<String>,
) -> String {
    let edited = [crate::agent::compact::compaction::EditedFile {
        path: path.into(),
        fragments: fragments.iter().map(|text| text.to_string()).collect(),
        receipt_sha,
        receipt_rows: Vec::new(),
    }];
    files_message(workspace, &edited, 128_000).map_or_else(String::new, |message| {
        message["content"].as_str().unwrap().to_owned()
    })
}

#[test]
fn edited_file_carry_shows_located_edit_lines_of_large_files_and_path_only_otherwise() {
    let root = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(root.path()).unwrap();
    let large: String = (1..=4_000)
        .map(|n| format!("fn line_{n}() {{}}\n"))
        .collect();
    // Two context lines above the edit are each larger than the file budget together.
    let wide: String = (1..=4_000)
        .map(|n| match n {
            2998 | 2999 => format!("// {}\n", "p".repeat(7_000)),
            _ => format!("fn line_{n}() {{}}\n"),
        })
        .collect();
    let crlf = large.replace('\n', "\r\n");
    for (name, text) in [
        ("big.rs", &large),
        ("wide.rs", &wide),
        ("big_crlf.rs", &crlf),
    ] {
        std::fs::write(root.path().join(name), text).unwrap();
    }
    std::fs::write(root.path().join("small.rs"), "fn small() {}\n").unwrap();
    let (good, wide_sha, crlf_sha) = (sha(&large), sha(&wide), sha(&crlf));
    // (file, located text, receipt, expectation). Recorded line numbers never enter the carry.
    let cases: Vec<(&str, Vec<&str>, Option<String>, &str)> = vec![
        (
            "big.rs",
            vec!["fn line_3000() {}"],
            Some(good.clone()),
            "receipt",
        ),
        (
            "big.rs",
            vec!["fn line_3000() {}"],
            Some(sha("stale")),
            "content",
        ),
        ("big.rs", vec!["fn line_3000() {}"], None, "content"),
        ("big.rs", vec!["fn gone() {}"], None, "omitted"),
        ("big.rs", vec!["fn line_1"], None, "omitted"),
        ("big.rs", vec![], Some(good), "omitted"),
        ("wide.rs", vec!["fn line_3000() {}"], Some(wide_sha), "core"),
        (
            "big_crlf.rs",
            vec!["fn line_3000() {}\nfn line_3001() {}"],
            Some(crlf_sha),
            "crlf",
        ),
        ("small.rs", vec![], None, "whole"),
        ("small.rs", vec!["fn small() {}"], None, "whole"),
    ];
    for (path, fragments, receipt, expected) in cases {
        let content = carried_file(&workspace, path, &fragments, receipt);
        assert!(
            content.len() < 16_384,
            "{expected}: {} bytes",
            content.len()
        );
        match expected {
            "receipt" => assert!(
                content.contains("lines=\"2997-3003\"")
                    && content.contains("location=\"edit receipt")
                    && content.contains("fn line_3000() {}\n")
                    && !content.contains("fn line_1() {}"),
                "{content}"
            ),
            "content" => assert!(
                content.contains("location=\"content match only")
                    && content.contains("fn line_3000() {}\n"),
                "{content}"
            ),
            "core" => assert!(
                content.contains("lines=\"3000-3000\"")
                    && content.contains("fn line_3000() {}")
                    && !content.contains("ppppp"),
                "the edit line survives long context lines: {content}"
            ),
            "crlf" => assert!(
                content.contains("fn line_3000() {}\nfn line_3001() {}")
                    && !content.contains('\r')
                    && content.contains("location=\"edit receipt"),
                "{content}"
            ),
            "whole" => assert!(
                content.contains("<file path=\"small.rs\">\nfn small() {}\n")
                    && !content.contains("lines="),
                "{content}"
            ),
            "omitted" => assert!(
                content.contains("omitted=") && !content.contains("fn line_"),
                "{content}"
            ),
            other => panic!("unknown expectation {other}"),
        }
    }
}

#[test]
fn compaction_uses_current_hashline_receipt_for_ambiguous_edit_and_drops_stale_rows() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .with_edit_mode(crate::config::types::EditMode::Hashline);
    let path = dir.path().join("large.txt");
    let original = "repeat this row\n".repeat(2400) + "replace this row\nend of file\n";
    std::fs::write(&path, &original).unwrap();
    let config = crate::config::types::Config::default();
    let ctx = crate::tools::integrations::ext::ToolContext {
        workspace: &workspace,
        config: &config,
        confirm: None,
        mutation_authorizer: Some(&|_, _, _| Ok(())),
    };
    let cancel = Default::default();
    let read = crate::tools::read_with_context(
        &ctx,
        &json!({"path":"large.txt:2400-2402"}),
        &cancel,
        8192,
    );
    assert!(!read.is_error, "{}", read.text);
    let header = read.text.lines().next().unwrap();
    let args = json!({"input":format!("{header}\nPUT 2401.=2401:\n+repeat this row")});
    let result = crate::tools::edit_with_context(&ctx, &args, None, &cancel);
    assert!(!result.is_error, "{}", result.text);
    let edited = crate::agent::compact::compaction::edited_this_turn(&[
        json!({"role":"assistant","tool_calls":[{"id":"one","function":{"name":"edit","arguments":args.to_string()}}]}),
        json!({"role":"tool","tool_call_id":"one","is_error":false,"content":result.text}),
    ]);
    let carry = || {
        files_message(&workspace, &edited, 128_000).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let content = carry();
    assert!(
        content.contains("end of file") && content.contains("lines=\"") && content.len() < 4096,
        "{content}"
    );
    std::fs::write(path, original).unwrap();
    let stale = carry();
    assert!(
        stale.contains("omitted=") && !stale.contains("repeat this row"),
        "{stale}"
    );
}

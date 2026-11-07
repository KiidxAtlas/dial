use crate::agent::compact::compaction::{
    CompactionOutcome, CompactionOutcomeKind, PreparedCompaction,
};
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;
use crate::config::compaction::{CompactionMethod, CompactionOptions};
use crate::storage::session::Session;
use serde_json::{Value, json};

fn thinking(message: &Value) -> bool {
    message["dial_provider_content"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|block| block["type"] == "thinking")
}

/// A client-side summary on a preserved-thinking model: retained turns lose only their signed
/// thinking (which the replaced history invalidated), and the files this turn edited are
/// re-attached from the workspace.
fn commit_soft_summary(model: &str) -> Vec<Value> {
    with_agent("", |agent| {
        agent.auth_config.provider = Some("anthropic".into());
        agent.auth_config.model = model.into();
        std::fs::write(agent.workspace.root().join("a.rs"), "fn edited() {}\n").unwrap();
        let signed = |text: &str, rest: Value| json!([{"type":"thinking","thinking":"","signature":format!("sig-{text}")}, rest]);
        for message in [
            json!({"role":"user","content":"survey the old code"}),
            json!({"role":"assistant","content":"old findings ".repeat(4_000)}),
            json!({"role":"user","content":"edit a.rs"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"w1","type":"function","function":{"name":"write","arguments":"{\"path\":\"a.rs\",\"content\":\"fn edited() {}\\n\"}"}}],
                "dial_provider_content":signed("w1", json!({"type":"tool_use","id":"w1","name":"write","input":{"path":"a.rs"}}))}),
            json!({"role":"tool","tool_call_id":"w1","content":"wrote a.rs"}),
            json!({"role":"assistant","content":"done",
                "dial_provider_content":signed("done", json!({"type":"text","text":"done"}))}),
        ] {
            agent.push(message).unwrap();
        }
        let snapshot = agent.compaction_snapshot(None).unwrap();
        let first_kept = snapshot
            .source
            .messages
            .iter()
            .position(|m| m["content"] == "edit a.rs")
            .unwrap();
        let prepared = PreparedCompaction {
            summary: "surveyed".into(),
            prefix: vec![Session::compaction_message("surveyed")],
            first_kept,
            method: CompactionMethod::Soft,
            outcome: CompactionOutcome {
                kind: CompactionOutcomeKind::Compaction,
                method: Some(CompactionMethod::Soft),
                tokens_before: 0,
                tokens_after: 0,
                retained_messages: 4,
                summarized_messages: 2,
                continue_allowed: false,
                native_usage: None,
                handoff_path: None,
                handoff_error: None,
                reset_entry_id: None,
                reset_digest: None,
                journal_warning: None,
            },
        };
        let mut backend = agent.backend.take().unwrap();
        agent
            .commit_compaction(
                &mut crate::agent::NullUi,
                &snapshot,
                prepared,
                &CompactionOptions::default(),
                None,
                Some(&mut backend),
            )
            .unwrap();
        agent.backend = Some(backend);
        agent.session.messages()
    })
}

#[test]
fn soft_compaction_unbinds_retained_thinking_only_on_preserved_thinking_models() {
    let preserved = commit_soft_summary("claude-opus-5-5");
    assert!(!preserved.iter().any(thinking), "{preserved:#?}");
    let write = preserved
        .iter()
        .find(|m| m["tool_calls"][0]["id"] == "w1")
        .unwrap();
    assert_eq!(
        write["dial_provider_content"],
        json!([{"type":"tool_use","id":"w1","name":"write","input":{"path":"a.rs"}}])
    );
    let other = commit_soft_summary("claude-opus-5");
    assert_eq!(other.iter().filter(|m| thinking(m)).count(), 2);
}

#[test]
fn compaction_reattaches_the_current_text_of_files_edited_this_turn() {
    let messages = commit_soft_summary("claude-opus-5");
    let files: Vec<_> = messages
        .iter()
        .filter(|m| m["dial_compaction_files"] == true)
        .collect();
    assert_eq!(files.len(), 1, "{messages:#?}");
    let text = files[0]["content"].as_str().unwrap();
    assert!(
        text.contains("<file path=\"a.rs\">\nfn edited() {}\n"),
        "{text}"
    );
    // The carry sits in the replacement prefix, before the retained turn it describes.
    let carry = messages
        .iter()
        .position(|m| m["dial_compaction_files"] == true)
        .unwrap();
    let retained = messages
        .iter()
        .position(|m| m["content"] == "edit a.rs")
        .unwrap();
    assert!(carry < retained);
}

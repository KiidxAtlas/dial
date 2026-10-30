use super::*;
use dial_llm::llm::batch::{Item, Provider};
fn plan() -> Plan {
    Plan {
        provider: Provider::Openai,
        model: "fixture".into(),
        max_tokens: 32,
        items: vec![Item {
            custom_id: "one".into(),
            prompt: "summary".into(),
        }],
    }
}
#[test]
fn private_preparation_survives_reopen_and_rejects_concurrent_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(temp.path()).unwrap();
    let job = ledger.prepare(plan()).unwrap();
    assert!(Ledger::open(temp.path()).is_err());
    drop(ledger);
    let ledger = Ledger::open(temp.path()).unwrap();
    let reopened = ledger.load(&job.id).unwrap();
    assert_eq!(reopened.state, State::Prepared);
    assert_eq!(reopened.plan.items[0].prompt, "summary");
    assert!(ledger.load("../../elsewhere").is_err());
}
#[test]
fn ambiguous_submission_is_durable_and_cannot_be_resubmitted() {
    let temp = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(temp.path()).unwrap();
    let mut job = ledger.prepare(plan()).unwrap();
    job.state = State::SubmissionUnknown;
    ledger.save(&job).unwrap();
    drop(ledger);
    let ledger = Ledger::open(temp.path()).unwrap();
    let job = ledger.load(&job.id).unwrap();
    assert_eq!(job.state, State::SubmissionUnknown);
    assert!(job.remote_id.is_none());
}

#[test]
fn archive_consumer_prepares_immutable_redacted_snapshots_without_application() {
    let temp = tempfile::tempdir().unwrap();
    let sessions = temp.path().join("projects/example-12345678/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let id = "1234567890abcdef";
    let source = sessions.join(format!("20260102_{id}.jsonl"));
    let header = serde_json::json!({"type":"session","id":id,"started":"2026-01-02T03:04:05.000Z","cwd":"/project","model":"fixture"});
    std::fs::write(&source, format!("{header}\n")).unwrap();
    let history = super::super::archive::PromptHistory::open(temp.path()).unwrap();
    history
        .add("review parser", Some("/project"), Some(id))
        .unwrap();
    let archive = super::super::archive::Archive::new(temp.path());
    let archive_plan =
        archive_summaries(&archive, Provider::Anthropic, "fixture", &[id.to_owned()]).unwrap();
    assert!(archive_plan.items[0].prompt.contains("review parser"));
    let ledger = Ledger::open(temp.path()).unwrap();
    let job = ledger.prepare(archive_plan).unwrap();
    history
        .add("newer change", Some("/project"), Some(id))
        .unwrap();
    assert!(
        !ledger.load(&job.id).unwrap().plan.items[0]
            .prompt
            .contains("newer change")
    );
    assert!(archive_summaries(&archive, Provider::Openai, "fixture", &["unknown".into()]).is_err());
    let mut secret_plan = plan();
    secret_plan.items[0].prompt = format!("Authorization: Bearer {}", "x".repeat(40));
    let redacted = ledger.prepare(secret_plan).unwrap();
    assert!(!redacted.plan.items[0].prompt.contains(&"x".repeat(40)));
}

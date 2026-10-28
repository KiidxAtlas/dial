use super::compute;
use crate::stats::Db;
use serde_json::json;
use std::fs;

#[test]
fn shouted_sentence_counts_as_yelling() {
    let metrics = compute("THIS IS COMPLETELY BROKEN AND I AM SO DONE WITH IT");
    assert!(metrics.yelling >= 1);
    assert!(metrics.flagged());
}

#[test]
fn profanity_anguish_negation_repetition_and_blame_are_detected() {
    assert!(compute("this is damn broken").profanity >= 1);
    assert!(compute("noooo that failed").anguish >= 1);
    assert!(compute("nope that is wrong").negation >= 1);
    assert!(compute("I already said this twice").repetition >= 1);
    assert!(compute("you didn't fix it").blame >= 1);
}

#[test]
fn neutral_message_is_not_flagged() {
    let metrics = compute("thanks, that worked perfectly");
    assert!(!metrics.flagged());
    assert!(metrics.chars > 0);
}

#[test]
fn pasted_prose_with_several_lines_is_not_scored() {
    let metrics = compute("damn\ndamn\ndamn\ndamn");
    assert_eq!(metrics.profanity, 0);
    assert!(!metrics.flagged());
    assert!(metrics.chars > 0);
}

#[test]
fn fenced_code_is_not_scored() {
    assert_eq!(compute("```\ndamn it\n```").profanity, 0);
}

#[test]
fn urls_are_not_scored() {
    assert_eq!(compute("see https://example.com/damn").profanity, 0);
}

#[test]
fn frustration_counts_user_messages_from_in_tree_transcripts() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("projects/demo-1234abcd/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let lines = [
        json!({"type":"session","id":"abcd1234","started":"2026-10-10T00:00:00.000Z","cwd":"/work/demo","model":"m1","version":"0"}),
        json!({"type":"message","id":"e1","parent":null,"ts":"2026-10-10T00:00:01.000Z","message":{"role":"user","content":"thanks"}}),
        json!({"type":"message","id":"e2","parent":"e1","ts":"2026-10-10T00:00:02.000Z","message":{"role":"user","content":"ugh this is damn broken"}}),
    ];
    fs::write(
        sessions.join("20261010T000000000Z_abcd1234.jsonl"),
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let mut db = Db::open_default(home.path()).unwrap();
    db.sync_sessions(home.path()).unwrap();
    let report = db.frustration(home.path(), None).unwrap();
    assert_eq!(report.sessions_scanned, 1);
    assert_eq!(report.sessions_unreadable, 0);
    assert_eq!(report.user_messages, 2);
    assert_eq!(report.flagged_messages, 1);
    assert_eq!(report.flagged_rate, Some(50.0));
}

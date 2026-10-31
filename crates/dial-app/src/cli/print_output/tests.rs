use super::*;
use serde_json::json;
use std::cell::Cell;

fn turn(text: &str) -> TurnReport {
    TurnReport {
        text: text.into(),
        outcome: TurnOutcome::Completed,
        steps: 1,
        interrupted: false,
        trajectory: None,
    }
}

fn answer_schema() -> Value {
    json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false})
}

#[test]
fn schema_file_errors_are_reported_before_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.json");
    assert!(
        load_schema(&missing)
            .unwrap_err()
            .to_string()
            .contains("cannot read")
    );

    let malformed = dir.path().join("malformed.json");
    std::fs::write(&malformed, "{not json").unwrap();
    assert!(
        load_schema(&malformed)
            .unwrap_err()
            .to_string()
            .contains("not valid JSON")
    );

    let unknown_type = dir.path().join("unknown.json");
    std::fs::write(&unknown_type, r#"{"type":"nonsense"}"#).unwrap();
    assert!(
        load_schema(&unknown_type)
            .unwrap_err()
            .to_string()
            .contains("invalid --output-schema")
    );

    let valid = dir.path().join("valid.json");
    std::fs::write(&valid, answer_schema().to_string()).unwrap();
    assert_eq!(load_schema(&valid).unwrap(), answer_schema());
}

#[test]
fn final_answers_are_checked_against_the_schema() {
    let schema = answer_schema();
    for (text, valid) in [
        (r#"{"answer":"ok"}"#, true),
        ("```json\n{\"answer\":\"ok\"}\n```", true),
        (r#"{"other":1}"#, false),
        ("plain prose", false),
    ] {
        assert_eq!(structured_answer(text, &schema).is_ok(), valid, "{text}");
    }
}

#[test]
fn a_rejected_answer_gets_bounded_corrections_then_fails_without_an_answer() {
    let schema = answer_schema();
    let calls = Cell::new(0);
    let checked = enforce(turn("nope"), &schema, 2, |message| {
        calls.set(calls.get() + 1);
        assert!(message.contains("Schema correction attempt"));
        Ok(turn("still nope"))
    })
    .unwrap();
    match checked {
        Checked::Rejected(report, why) => {
            assert!(report.text.is_empty(), "a rejected answer is not printed");
            assert_eq!(report.outcome, TurnOutcome::Incomplete);
            assert!(!why.is_empty());
        }
        Checked::Valid(..) => panic!("an invalid answer must not pass"),
    }
    assert_eq!(calls.get(), 2, "the budget bounds the correction turns");
}

#[test]
fn a_correction_that_fixes_the_answer_is_accepted_as_compact_json() {
    let schema = answer_schema();
    let checked = enforce(turn("nope"), &schema, 1, |_| {
        Ok(turn("```json\n{\"answer\": \"fixed\"}\n```"))
    })
    .unwrap();
    match checked {
        Checked::Valid(report, value) => {
            assert_eq!(value, json!({"answer":"fixed"}));
            assert_eq!(report.text, r#"{"answer":"fixed"}"#);
        }
        Checked::Rejected(_, why) => panic!("expected a valid answer: {why}"),
    }
}

#[test]
fn an_interrupted_turn_stops_without_spending_the_budget() {
    let schema = answer_schema();
    let calls = Cell::new(0);
    let interrupted = TurnReport {
        interrupted: true,
        ..turn("")
    };
    let checked = enforce(interrupted, &schema, 3, |_| {
        calls.set(calls.get() + 1);
        Ok(turn(""))
    })
    .unwrap();
    assert!(matches!(checked, Checked::Rejected(..)));
    assert_eq!(calls.get(), 0);
}

#[test]
fn last_message_is_written_only_when_there_is_an_answer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("answer.txt");

    assert!(!write_last_message(&path, "  \n").unwrap());
    assert!(!path.exists(), "no answer must not create the file");

    std::fs::write(&path, "previous").unwrap();
    assert!(!write_last_message(&path, "").unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous");

    assert!(write_last_message(&path, "final answer").unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "final answer\n");
}

use super::*;

#[test]
fn kickoff_keeps_the_task_verbatim_and_asks_for_a_checkable_contract() {
    let text = kickoff("Fix the parser crash on empty input");
    assert!(text.starts_with("Fix the parser crash on empty input\n\n"));
    for required in [
        "working_state",
        "constraints",
        "status `open`",
        "observable check",
        "ask before starting",
        "`satisfied` only with evidence",
        "`blocked`",
    ] {
        assert!(text.contains(required), "missing {required:?}: {text}");
    }
}

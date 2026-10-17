use super::*;
#[test]
fn compaction_is_bounded_retrievable_and_preserves_verdicts() {
    let text = format!(
        "{}\nrunning 12 tests\ntest result: ok. 12 passed\n[exit code: 0]",
        "build progress\n".repeat(1000)
    );
    let compact = compact_success(&text, "artifact://check").unwrap();
    assert!(
        compact.len() < 2300
            && compact.contains("12 passed")
            && compact.contains("artifact://check")
    );
    assert!(
        compact_success(
            &text.replace("[exit code: 0]", "[exit code: 1]"),
            "artifact://check"
        )
        .is_none()
    );
    assert!(
        compact_success(
            &text.replace("running 12 tests", "running 0 tests"),
            "artifact://check"
        )
        .is_none()
    );
}

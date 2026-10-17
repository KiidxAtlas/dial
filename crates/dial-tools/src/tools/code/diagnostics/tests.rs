use super::*;

#[test]
fn recognizes_common_toolchain_failures_but_not_passing_summaries() {
    for line in [
        "error[E0308]: mismatched types",
        "error: could not compile `dial` (lib) due to 1 previous error",
        "  --> src/agent/mod.rs:42:9",
        "src/main.c:3:5: error: expected ';' before '}' token",
        "app.py:12: error: Incompatible return value type",
        "thread 'tests::x' panicked at src/lib.rs:9:5:",
        "Traceback (most recent call last):",
        "ValueError: invalid literal for int() with base 10: 'x'",
        "E       assert 1 == 2",
        "FAILED tests/test_app.py::test_parse - AssertionError",
        "--- FAIL: TestParse (0.00s)",
        "test parser::handles_emoji ... FAILED",
        "  ● parser › handles emoji",
    ] {
        assert!(is_diagnostic(line), "{line}");
    }
    for line in [
        "test result: ok. 12 passed; 0 failed",
        "   Compiling dial v0.1.0",
        "warning: unused variable: `x`",
        "test a ... ok",
        "no errors found",
    ] {
        assert!(!is_diagnostic(line), "{line}");
    }
}

#[test]
fn collector_joins_split_lines_and_respects_its_budget() {
    let mut collector = Collector::new(60);
    collector.feed(b"noise\nerror[E0");
    collector.feed(b"308]: mismatched types\n  --> src/a.rs:1:1\nmore noise\n");
    collector.feed(b"error: second failure that does not fit in the remaining budget\n");
    collector.finish();
    let (kept, lines, overflow) = collector.kept();
    assert_eq!(kept, "error[E0308]: mismatched types\n  --> src/a.rs:1:1\n");
    assert_eq!((lines, overflow), (2, 1));
}

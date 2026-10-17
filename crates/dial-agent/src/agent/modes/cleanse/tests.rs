use super::*;

fn diagnostic(file: Option<&str>, line: Option<u64>, message: &str) -> Diagnostic {
    Diagnostic {
        checker: "tsc".into(),
        file: file.map(str::to_owned),
        line,
        column: None,
        end_line: None,
        end_column: None,
        code: Some("TS1".into()),
        severity: Severity::Error,
        message: message.into(),
        suggestion: Some("fix it".into()),
    }
}

#[test]
fn groups_are_heaviest_first_and_batches_respect_budget() {
    let diagnostics = [
        diagnostic(Some("b.ts"), Some(1), "x"),
        diagnostic(Some("a.ts"), Some(1), "y"),
        diagnostic(Some("a.ts"), Some(2), "z"),
    ];
    let mut groups = group_by_file(&diagnostics);
    assert_eq!(groups[0].file, "a.ts");
    assert_eq!(groups[0].weight, 10);
    let batch = take_batch(&mut groups, 5);
    assert_eq!(batch.len(), 1);
    assert_eq!(groups.len(), 1);
}

#[test]
fn dedupe_drops_repeats_regardless_of_checker() {
    let mut other = diagnostic(Some("a.ts"), Some(1), "x");
    other.checker = "other".into();
    let unique = dedupe_project(vec![diagnostic(Some("a.ts"), Some(1), "x"), other]);
    assert_eq!(unique.len(), 1);
}

#[test]
fn formats_location_code_and_suggestion() {
    let text = format_diagnostics(&[diagnostic(Some("a.ts"), Some(3), "boom")]);
    assert_eq!(
        text,
        "- [error] a.ts:3 — tsc TS1: boom\n  Suggested fix: fix it"
    );
}

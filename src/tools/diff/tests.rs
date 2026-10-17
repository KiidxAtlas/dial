use super::*;
#[test]
fn changing_middle_shows_context_and_both_versions() {
    let p = preview("keep\nold\nend\n", "keep\nnew\nend\n");
    assert!(
        p.contains("(+1 -1 lines)\n@@ -1,3 +1,3 @@\n keep\n-old\n+new\n end\n"),
        "{p}"
    );
}
#[test]
fn separate_edits_stay_separate_hunks_without_unchanged_lines_marked() {
    let before: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    let after = before
        .replace("line 5\n", "line five\n")
        .replace("line 35\n", "line thirty-five\n");
    let p = preview(&before, &after);
    assert_eq!(p.matches("@@ ").count(), 2, "{p}");
    assert!(
        p.contains("@@ -3,5 +3,5 @@\n line 3\n line 4\n-line 5\n+line five\n"),
        "{p}"
    );
    assert!(
        !p.contains("-line 20") && !p.contains("+line 20"),
        "unchanged lines are never shown as changed"
    );
}
#[test]
fn bounds_large_replacements_and_reports_newline_changes() {
    assert!(preview(&"old\n".repeat(100), "new\n").contains("truncated"));
    assert!(preview("same", "same\n").contains("final newline added"));
}

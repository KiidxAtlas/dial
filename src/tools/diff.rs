//! Bounded unified diffs for edit and write results: real hunks with a little context, so the model and
//! the transcript see exactly which lines changed, and separate edits in one file stay separate hunks.
use similar::{Algorithm, ChangeTag, TextDiff};
use std::time::Duration;

const MAX_ROWS: usize = 60;
const MAX_BYTES: usize = 8192;
/// Unchanged lines shown around each change.
const CONTEXT: usize = 2;
/// A pathological input gets a coarser (still correct) diff instead of stalling the edit.
const DIFF_TIMEOUT: Duration = Duration::from_millis(200);

pub(super) fn preview(before: &str, after: &str) -> String {
    if before == after { return String::new(); }
    let diff = TextDiff::configure().algorithm(Algorithm::Myers).timeout(DIFF_TIMEOUT).diff_lines(before, after);
    let (mut added, mut removed) = (0, 0);
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let mut out = format!("\nchange preview (+{added} -{removed} lines)\n");
    let mut rows = 0;
    for group in diff.grouped_ops(CONTEXT) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
        let (old, new) = (first.old_range().start..last.old_range().end, first.new_range().start..last.new_range().end);
        out.push_str(&format!("@@ -{},{} +{},{} @@\n", old.start + 1, old.len(), new.start + 1, new.len()));
        for op in &group {
            for change in diff.iter_changes(op) {
                let line = change.value().trim_end_matches(['\n', '\r']);
                if rows == MAX_ROWS || out.len() + line.len() + 2 > MAX_BYTES {
                    out.push_str("[diff preview truncated; read the changed region for full content]\n");
                    return out;
                }
                out.push(match change.tag() { ChangeTag::Delete => '-', ChangeTag::Insert => '+', ChangeTag::Equal => ' ' });
                out.push_str(line);
                out.push('\n');
                rows += 1;
            }
        }
    }
    if before.ends_with('\n') != after.ends_with('\n') {
        out.push_str(if after.ends_with('\n') { "[final newline added]\n" } else { "[final newline removed]\n" });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changing_middle_shows_context_and_both_versions() {
        let p = preview("keep\nold\nend\n", "keep\nnew\nend\n");
        assert!(p.contains("(+1 -1 lines)\n@@ -1,3 +1,3 @@\n keep\n-old\n+new\n end\n"), "{p}");
    }
    #[test]
    fn separate_edits_stay_separate_hunks_without_unchanged_lines_marked() {
        let before: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let after = before.replace("line 5\n", "line five\n").replace("line 35\n", "line thirty-five\n");
        let p = preview(&before, &after);
        assert_eq!(p.matches("@@ ").count(), 2, "{p}");
        assert!(p.contains("@@ -3,5 +3,5 @@\n line 3\n line 4\n-line 5\n+line five\n"), "{p}");
        assert!(!p.contains("-line 20") && !p.contains("+line 20"), "unchanged lines are never shown as changed");
    }
    #[test]
    fn bounds_large_replacements_and_reports_newline_changes() {
        assert!(preview(&"old\n".repeat(100), "new\n").contains("truncated"));
        assert!(preview("same", "same\n").contains("final newline added"));
    }
}

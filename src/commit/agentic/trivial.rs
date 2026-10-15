//! Shortcut for diffs that are only whitespace or only import churn.

use std::sync::LazyLock;

use regex::Regex;

static IMPORT_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[-+]\s*(import\s|from\s|export\s.*from|require\(|module\.exports)")
        .expect("valid regex")
});
static EMPTY_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]\s*$").expect("valid regex"));

/// A diff classified as trivial, with the commit type and summary to use without the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrivialChange {
    pub(crate) commit_type: &'static str,
    pub(crate) summary: &'static str,
}

/// Classify `diff` as whitespace-only (`style`, "formatted code") or import-only
/// (`style`, "reorganized imports"). Returns `None` for anything else, including empty diffs.
pub(crate) fn detect_trivial_change(diff: &str) -> Option<TrivialChange> {
    let content: Vec<&str> = diff
        .split('\n')
        .filter(|line| line.starts_with('+') || line.starts_with('-'))
        .filter(|line| {
            !line.starts_with("+++") && !line.starts_with("---") && !line.starts_with("@@")
        })
        .collect();
    if content.is_empty() {
        return None;
    }
    if is_only_whitespace(&content) {
        return Some(TrivialChange {
            commit_type: "style",
            summary: "formatted code",
        });
    }
    if is_only_imports(&content) {
        return Some(TrivialChange {
            commit_type: "style",
            summary: "reorganized imports",
        });
    }
    None
}

fn is_only_whitespace(lines: &[&str]) -> bool {
    // Whitespace-only means the removed and added text match once all whitespace is dropped.
    let squashed = |prefix: char| -> String {
        lines
            .iter()
            .filter(|line| line.starts_with(prefix))
            .flat_map(|line| line[1..].chars())
            .filter(|c| !c.is_whitespace())
            .collect()
    };
    squashed('-') == squashed('+')
}

fn is_only_imports(lines: &[&str]) -> bool {
    lines
        .iter()
        .all(|line| EMPTY_LINE.is_match(line) || IMPORT_LINE.is_match(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_only_diff_is_style() {
        let diff = "@@ -1,2 +1,2 @@\n-  x\n+ x\n";
        let trivial = detect_trivial_change(diff).expect("trivial");
        assert_eq!(trivial.summary, "formatted code");
        assert_eq!(trivial.commit_type, "style");
    }

    #[test]
    fn import_only_diff_is_reorganized_imports() {
        let diff = "@@ -1 +1 @@\n-import a from 'a';\n+import b from 'b';\n";
        let trivial = detect_trivial_change(diff).expect("trivial");
        assert_eq!(trivial.summary, "reorganized imports");
    }

    #[test]
    fn real_change_is_not_trivial() {
        assert!(detect_trivial_change("@@ -1 +1 @@\n-let x = 1;\n+let x = 2;\n").is_none());
        assert!(detect_trivial_change("").is_none());
    }
}

//! Rules applied to proposed commits before they are accepted.

use crate::commit::markdown::strip_type_prefix;
use crate::commit::normalization::normalize_commit_unicode;
use crate::commit::validation::is_past_tense_first_word;

use super::state::{ChangelogCategory, ConventionalDetail};

pub(crate) const SUMMARY_MAX_CHARS: usize = 72;
pub(crate) const MAX_DETAIL_ITEMS: usize = 6;
pub(crate) const DETAIL_MAX_CHARS: usize = 120;

const FILLER_WORDS: [&str; 6] = [
    "comprehensive",
    "various",
    "several",
    "improved",
    "enhanced",
    "better",
];
const META_PHRASES: [&str; 4] = [
    "this commit",
    "this change",
    "updated code",
    "modified files",
];

/// Errors (block acceptance) and warnings (surfaced to the model) from one check.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    pub(crate) errors: Vec<String>,
    pub(crate) warnings: Vec<String>,
}

/// Strip a leading `type(scope):` prefix, normalize unicode, and collapse whitespace.
pub(crate) fn normalize_summary(summary: &str, commit_type: &str, scope: Option<&str>) -> String {
    let stripped = strip_type_prefix(summary, Some(commit_type), scope);
    normalize_commit_unicode(&stripped)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Summary-level rules: length, no trailing period, single line, past tense, filler and meta phrases.
pub(crate) fn validate_summary_rules(summary: &str) -> Checked {
    let mut checked = Checked::default();
    if summary.trim().is_empty() {
        checked.errors.push("Summary is empty".to_owned());
    }
    if summary.chars().count() > SUMMARY_MAX_CHARS {
        checked
            .errors
            .push(format!("Summary exceeds {SUMMARY_MAX_CHARS} characters"));
    }
    if summary.trim_end().ends_with('.') {
        checked
            .errors
            .push("Summary must not end with a period".to_owned());
    }
    if summary.contains('\n') {
        checked
            .errors
            .push("Summary must be a single line".to_owned());
    }
    let first = summary
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if !is_past_tense_first_word(&first) {
        checked
            .errors
            .push("Summary must start with a past-tense verb".to_owned());
    }
    let lower = summary.to_lowercase();
    for word in FILLER_WORDS {
        if lower.contains(word) {
            checked.warnings.push(format!("Avoid filler word: {word}"));
        }
    }
    for phrase in META_PHRASES {
        if lower.contains(phrase) {
            checked
                .warnings
                .push(format!("Avoid meta phrase: {phrase}"));
        }
    }
    checked
}

/// Scope rules: at most two `/`-separated lowercase segments of `[a-z0-9][a-z0-9-_]*`.
pub(crate) fn validate_scope(scope: Option<&str>) -> Vec<String> {
    let Some(scope) = scope else {
        return Vec::new();
    };
    let mut errors = Vec::new();
    let segments: Vec<&str> = scope.split('/').collect();
    if segments.len() > 2 {
        errors.push("Scope may contain at most two segments".to_owned());
    }
    for segment in segments {
        if segment.is_empty() {
            errors.push("Scope segments cannot be empty".to_owned());
            continue;
        }
        if segment != segment.to_lowercase() {
            errors.push("Scope must be lowercase".to_owned());
        }
        if !is_valid_scope_segment(segment) {
            errors.push(format!("Scope segment has invalid characters: {segment}"));
        }
    }
    errors
}

fn is_valid_scope_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Detail rules: non-empty, ends with a period, at most 120 characters.
pub(crate) fn validate_details(details: &[ConventionalDetail]) -> Vec<String> {
    let mut errors = Vec::new();
    for detail in details {
        let text = detail.text.trim();
        if text.is_empty() {
            errors.push("Detail text is empty".to_owned());
            continue;
        }
        if !text.ends_with('.') {
            errors.push(format!("Detail must end with a period: {}", detail.text));
        }
        if detail.text.chars().count() > DETAIL_MAX_CHARS {
            errors.push(format!(
                "Detail exceeds {DETAIL_MAX_CHARS} characters: {}",
                detail.text
            ));
        }
    }
    errors
}

/// Full analysis check: scope plus details.
pub(crate) fn validate_analysis(
    scope: Option<&str>,
    details: &[ConventionalDetail],
) -> Vec<String> {
    let mut errors = validate_scope(scope);
    errors.extend(validate_details(details));
    errors
}

/// Keep at most [`MAX_DETAIL_ITEMS`] details, prioritised by keyword score, original order kept.
pub(crate) fn cap_details(
    details: &[ConventionalDetail],
) -> (Vec<ConventionalDetail>, Vec<String>) {
    if details.len() <= MAX_DETAIL_ITEMS {
        return (details.to_vec(), Vec::new());
    }
    let mut scored: Vec<(usize, u32)> = details
        .iter()
        .enumerate()
        .map(|(index, detail)| (index, score_detail(&detail.text)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut keep: Vec<usize> = scored
        .iter()
        .take(MAX_DETAIL_ITEMS)
        .map(|(index, _)| *index)
        .collect();
    keep.sort_unstable();
    let kept = keep
        .into_iter()
        .map(|index| details[index].clone())
        .collect();
    (
        kept,
        vec![format!(
            "Capped detail list to {MAX_DETAIL_ITEMS} items based on priority scoring."
        )],
    )
}

fn score_detail(text: &str) -> u32 {
    let lower = text.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| lower.contains(word));
    let mut score = 0;
    if has(&["security", "vulnerability", "exploit", "cve"]) {
        score += 100;
    }
    if has(&["breaking", "incompatible"]) {
        score += 90;
    }
    if has(&[
        "performance",
        "optimization",
        "optimiz",
        "latency",
        "throughput",
    ]) {
        score += 80;
    }
    if has(&["bug", "fix", "crash", "panic", "regression", "failure"]) {
        score += 70;
    }
    if has(&["api", "interface", "public", "export"]) {
        score += 50;
    }
    if has(&["user", "client", "customer"]) {
        score += 40;
    }
    if has(&["deprecated", "removed", "delete"]) {
        score += 35;
    }
    score
}

/// Type-specific file expectations. `files` are the changed paths, `diff_text` the staged diff.
pub(crate) fn validate_type_consistency(
    commit_type: &str,
    files: &[String],
    diff_text: &str,
    summary: &str,
    details: &[ConventionalDetail],
) -> Checked {
    let mut checked = Checked::default();
    let lower_files: Vec<String> = files.iter().map(|file| file.to_lowercase()).collect();
    let has_docs = lower_files.iter().any(|file| {
        [".md", ".mdx", ".adoc", ".rst"]
            .iter()
            .any(|ext| file.ends_with(ext))
    });
    let has_tests = lower_files.iter().any(|file| is_test_path(file));
    let has_ci = lower_files
        .iter()
        .any(|file| file.starts_with(".github/workflows/") || file.starts_with(".gitlab-ci"));
    let has_build = lower_files.iter().any(|file| {
        ["cargo.toml", "package.json", "makefile"]
            .iter()
            .any(|candidate| file.ends_with(candidate))
    });
    let has_perf_evidence = lower_files
        .iter()
        .any(|file| file.contains("bench") || file.contains("perf"));
    let detail_text: String = details
        .iter()
        .map(|detail| detail.text.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let haystack = format!("{} {}", summary.to_lowercase(), detail_text);
    let has_perf_keywords = [
        "performance",
        "optimiz",
        "latency",
        "throughput",
        "benchmark",
    ]
    .iter()
    .any(|word| haystack.contains(word));

    match commit_type {
        "docs" if !has_docs => checked
            .errors
            .push("Docs commit should include documentation file changes".to_owned()),
        "test" if !has_tests => checked
            .errors
            .push("Test commit should include test file changes".to_owned()),
        "ci" if !has_ci => checked
            .errors
            .push("CI commit should include CI configuration changes".to_owned()),
        "build" if !has_build => checked
            .errors
            .push("Build commit should include build-related files".to_owned()),
        "refactor" if diff_text.contains("\nnew file mode ") => checked
            .warnings
            .push("Refactor commit adds new files; consider feat if new functionality".to_owned()),
        "perf" if !has_perf_evidence && !has_perf_keywords => checked
            .warnings
            .push("Perf commit lacks benchmark or performance keywords".to_owned()),
        _ => {}
    }
    checked
}

/// True for a `test`/`tests`/`__tests__` path segment, or a `_test.`, `.test.`, or `.spec.` infix.
pub(crate) fn is_test_path(file: &str) -> bool {
    let in_test_dir = file
        .split('/')
        .any(|segment| matches!(segment, "test" | "tests" | "__tests__"));
    in_test_dir || file.contains("_test.") || file.contains(".test.") || file.contains(".spec.")
}

/// Map a raw changelog category string onto the enum, reporting unknown names as errors.
pub(crate) fn parse_category(raw: &str) -> Result<ChangelogCategory, String> {
    ChangelogCategory::parse(raw).ok_or_else(|| format!("Unknown changelog category: {raw}"))
}

#[cfg(test)]
mod tests;

//! Commit message validation: past-tense summaries, length limits, type/file consistency.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::LazyLock,
};

use serde::Deserialize;

use super::{
    commit_types::{COMMIT_TYPE_ORDER, CommitMessage},
    config::GenerationConfig,
    text::first_word,
};

#[derive(Deserialize)]
struct ValidationData {
    past_tense: Vec<(String, String)>,
    irregular_past: Vec<String>,
    ed_blocklist: Vec<String>,
    d_blocklist: Vec<String>,
    code_extensions: Vec<String>,
    doc_extensions: Vec<String>,
    filler_words: Vec<String>,
    meta_phrases: Vec<String>,
    body_present_tense: Vec<String>,
}

struct Tables {
    past_by_present: HashMap<String, String>,
    irregular_past: HashSet<String>,
    ed_blocklist: HashSet<String>,
    d_blocklist: HashSet<String>,
    code_extensions: HashSet<String>,
    doc_extensions: HashSet<String>,
    filler_words: Vec<String>,
    meta_phrases: Vec<String>,
    body_present_tense: HashSet<String>,
}

static TABLES: LazyLock<Tables> = LazyLock::new(|| {
    let data: ValidationData = serde_json::from_str(include_str!("resources/validation_data.json"))
        .unwrap_or_else(|error| panic!("validation_data.json is invalid: {error}"));
    let past_by_present: HashMap<String, String> = data
        .past_tense
        .iter()
        .map(|(present, past)| (present.to_lowercase(), past.to_lowercase()))
        .collect();
    let mut irregular_past: HashSet<String> = data
        .irregular_past
        .iter()
        .map(|word| word.to_lowercase())
        .collect();
    for (present, past) in &past_by_present {
        if present == past {
            irregular_past.insert(past.clone());
        }
    }
    Tables {
        past_by_present,
        irregular_past,
        ed_blocklist: data
            .ed_blocklist
            .iter()
            .map(|word| word.to_lowercase())
            .collect(),
        d_blocklist: data
            .d_blocklist
            .iter()
            .map(|word| word.to_lowercase())
            .collect(),
        code_extensions: data
            .code_extensions
            .iter()
            .map(|ext| ext.to_lowercase())
            .collect(),
        doc_extensions: data
            .doc_extensions
            .iter()
            .map(|ext| ext.to_lowercase())
            .collect(),
        filler_words: data
            .filler_words
            .iter()
            .map(|word| word.to_lowercase())
            .collect(),
        meta_phrases: data
            .meta_phrases
            .iter()
            .map(|phrase| phrase.to_lowercase())
            .collect(),
        body_present_tense: data
            .body_present_tense
            .iter()
            .map(|word| word.to_lowercase())
            .collect(),
    }
});

/// One validation finding. `code` identifies the rule; `message` is shown to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidationIssue {
    pub code: &'static str,
    pub message: String,
}

/// Errors block acceptance; warnings are advisory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ValidationReport {
    pub errors: Vec<ValidationIssue>,
    pub warnings: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub(crate) fn ok(&self) -> bool {
        self.errors.is_empty()
    }

    fn error(&mut self, code: &'static str, message: String) {
        self.errors.push(ValidationIssue { code, message });
    }

    fn warning(&mut self, code: &'static str, message: String) {
        self.warnings.push(ValidationIssue { code, message });
    }

    fn extend(&mut self, other: ValidationReport) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }

    pub(crate) fn has_error(&self, code: &str) -> bool {
        self.errors.iter().any(|issue| issue.code == code)
    }

    pub(crate) fn error_messages(&self) -> String {
        self.errors
            .iter()
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// True when `word` (any case) is a known past-tense English verb.
pub(crate) fn is_past_tense_verb(word: &str) -> bool {
    let lower = word.to_lowercase();
    let tables = &*TABLES;
    if tables
        .past_by_present
        .iter()
        .any(|(present, past)| *past == lower && *present != lower)
    {
        return true;
    }
    if lower.ends_with("ed") {
        return !tables.ed_blocklist.contains(&lower);
    }
    if lower.chars().count() >= 4 && lower.ends_with('d') {
        let before = lower.chars().rev().nth(1);
        if matches!(before, Some('a' | 'e' | 'i' | 'o' | 'u')) {
            return !tables.d_blocklist.contains(&lower);
        }
    }
    tables.irregular_past.contains(&lower)
}

/// Past-tense form of a present-tense verb from the table.
pub(crate) fn present_to_past(word: &str) -> Option<String> {
    TABLES.past_by_present.get(&word.to_lowercase()).cloned()
}

/// Leading ASCII letters of `token`, lowercased; `None` when empty or all uppercase (an acronym).
pub(crate) fn verb_stem(token: &str) -> Option<String> {
    let letters = leading_letters(token);
    if letters.is_empty() || letters.chars().all(|ch| ch.is_ascii_uppercase()) {
        return None;
    }
    Some(letters.to_lowercase())
}

fn leading_letters(text: &str) -> &str {
    let end = text
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_alphabetic())
        .map_or(text.len(), |(index, _)| index);
    &text[..end]
}

/// True when the first word of a summary is a past-tense verb (including `re-` compounds).
pub(crate) fn is_past_tense_first_word(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if is_past_tense_verb(token) {
        return true;
    }
    if let Some(stem) = verb_stem(token)
        && is_past_tense_verb(&stem)
    {
        return true;
    }
    let lower = token.to_lowercase();
    if let Some(rest) = lower.strip_prefix("re-") {
        let inner = leading_letters(rest).to_lowercase();
        return !inner.is_empty()
            && (is_past_tense_verb(&inner) || present_to_past(&inner).is_some());
    }
    false
}

/// Replace a present-tense first word with its past form, keeping the rest of the summary.
pub(crate) fn repair_summary_tense(summary: &str) -> String {
    let trimmed = summary.trim();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    match present_to_past(&trimmed[..end]) {
        Some(past) => format!("{past}{}", &trimmed[end..]),
        None => trimmed.to_owned(),
    }
}

/// Validate a standalone summary: non-empty, past-tense first word, type not repeated.
pub(crate) fn validate_summary_quality(
    summary: &str,
    commit_type: &str,
    stat: &str,
) -> ValidationReport {
    let trimmed = summary.trim();
    let mut report = ValidationReport::default();
    if trimmed.is_empty() {
        report.error("empty_summary", "summary is empty".to_owned());
        return report;
    }
    report.extend(validate_summary_content(trimmed, commit_type, stat));
    report
}

fn validate_summary_content(summary: &str, commit_type: &str, stat: &str) -> ValidationReport {
    let mut report = ValidationReport::default();
    let first = first_word(summary);
    if first.is_empty() {
        report.error(
            "summary_missing_word",
            "Summary must begin with a word".to_owned(),
        );
        return report;
    }
    if !is_past_tense_first_word(first) {
        report.error(
            "present_tense_first_word",
            format!("Summary must start with a past-tense verb (ending in -ed/-d or irregular). Got \"{first}\""),
        );
    }
    if first.to_lowercase() == commit_type {
        report.error(
            "type_word_repetition",
            format!("Summary repeats the commit type \"{commit_type}\""),
        );
    }
    let lowered = summary.to_lowercase();
    for filler in &TABLES.filler_words {
        if contains_phrase(&lowered, filler) {
            report.warning(
                "filler_word",
                format!("Summary contains filler word \"{filler}\""),
            );
        }
    }
    for phrase in &TABLES.meta_phrases {
        if contains_phrase(&lowered, phrase) {
            report.warning(
                "meta_phrase",
                format!("Summary contains meta-phrase \"{phrase}\"; describe what changed"),
            );
        }
    }
    if !stat.trim().is_empty() {
        report.extend(summary_file_mismatch(stat, commit_type));
    }
    report
}

/// True when `phrase` occurs in `lowered` as whole words, so "this pr" does not match "this proxy".
fn contains_phrase(lowered: &str, phrase: &str) -> bool {
    lowered.match_indices(phrase).any(|(start, _)| {
        let end = start + phrase.len();
        let before = lowered[..start].chars().next_back();
        let after = lowered[end..].chars().next();
        before.is_none_or(|ch| !ch.is_alphanumeric())
            && after.is_none_or(|ch| !ch.is_alphanumeric())
    })
}

/// Full validation of a message the model produced.
pub(crate) fn validate_commit_message(
    message: &CommitMessage,
    config: &GenerationConfig,
    stat: &str,
    project_names: &[String],
) -> ValidationReport {
    let mut report = ValidationReport::default();
    if !COMMIT_TYPE_ORDER.contains(&message.commit_type.as_str()) {
        report.error(
            "invalid_type",
            format!(
                "Invalid commit type: \"{}\". Must be one of: {}",
                message.commit_type,
                COMMIT_TYPE_ORDER.join(", ")
            ),
        );
    }
    if let Some(scope) = &message.scope {
        if scope.is_empty() {
            report.error("empty_scope", "Scope cannot be empty".to_owned());
        } else if project_names
            .iter()
            .any(|name| normalize_project_name(name) == normalize_project_name(scope))
        {
            report.error(
                "project_name_scope",
                format!(
                    "Scope \"{scope}\" is the project name; omit scope for project-wide changes"
                ),
            );
        }
    }
    let summary = message.summary.as_str();
    if summary.is_empty() {
        report.error("empty_summary", "Summary cannot be empty".to_owned());
    }
    if summary.ends_with('.') {
        report.error(
            "trailing_period",
            "Summary must NOT end with a period (conventional commits style)".to_owned(),
        );
    }
    let scope_part = message.scope.as_ref().map_or(0, |scope| scope.len() + 2);
    let first_line = message.commit_type.len() + scope_part + 2 + summary.len();
    if first_line > config.summary_hard_limit {
        report.error(
            "summary_too_long",
            format!(
                "Summary line exceeds hard limit: {first_line} > {} chars",
                config.summary_hard_limit
            ),
        );
    } else if first_line > config.summary_soft_limit {
        report.warning(
            "summary_soft_limit",
            format!(
                "Summary line exceeds soft limit: {first_line} > {} chars",
                config.summary_soft_limit
            ),
        );
    } else if first_line > config.summary_guideline {
        report.warning(
            "summary_guideline",
            format!(
                "Summary line exceeds guideline: {first_line} > {} chars",
                config.summary_guideline
            ),
        );
    }
    if !summary.is_empty() {
        report.extend(validate_summary_content(
            summary,
            &message.commit_type,
            stat,
        ));
    }
    for item in &message.body {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        if TABLES
            .body_present_tense
            .contains(&first_word(trimmed).to_lowercase())
        {
            report.warning(
                "present_tense_body_item",
                format!("Body item uses present tense: \"{trimmed}\""),
            );
        }
        if !trimmed.ends_with('.') {
            report.warning(
                "missing_period_body_item",
                format!("Body item is missing a period: \"{trimmed}\""),
            );
        }
    }
    if !stat.trim().is_empty() {
        report.extend(type_scope_consistency(
            &message.commit_type,
            stat,
            &message.body,
        ));
    }
    report
}

fn normalize_project_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_')
        .collect()
}

/// File paths named by a `git diff --stat` block (create-mode rows contribute their path too).
pub(crate) fn stat_paths(stat: &str) -> Vec<String> {
    stat.lines()
        .map(str::trim)
        .filter_map(|line| {
            if let Some(rest) = line.strip_prefix("create mode") {
                // "create mode <mode> <path>": the path may contain spaces.
                let path = rest.trim_start().split_once(' ')?.1;
                return unquote_stat_path(path);
            }
            // Stat rows are "<path> | <count>"; the summary line ("N files changed") has no '|'.
            let (path, _) = line.split_once('|')?;
            unquote_stat_path(path)
        })
        .collect()
}

/// Git C-quotes unusual names in stat output (`"caf\303\251.md"`). The quotes are stripped so
/// extensions and directory names still match; octal escapes are left as-is.
fn unquote_stat_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let path = trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(trimmed);
    (!path.is_empty()).then(|| path.to_owned())
}

fn extension_of(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_lowercase)
        .filter(|ext| !ext.is_empty())
}

fn summary_file_mismatch(stat: &str, commit_type: &str) -> ValidationReport {
    let mut report = ValidationReport::default();
    let paths = stat_paths(stat);
    if paths.is_empty() {
        return report;
    }
    let total = paths.len();
    let extensions: Vec<Option<String>> = paths.iter().map(|path| extension_of(path)).collect();
    let markdown = extensions
        .iter()
        .filter(|ext| ext.as_deref() == Some("md"))
        .count();
    if markdown * 100 / total > 80 && commit_type != "docs" {
        report.warning(
            "markdown_type_mismatch",
            format!(
                "Type mismatch: {}% .md files but type is \"{commit_type}\"; consider docs",
                markdown * 100 / total
            ),
        );
    }
    let code = extensions
        .iter()
        .filter(|ext| {
            ext.as_ref()
                .is_some_and(|ext| TABLES.code_extensions.contains(ext))
        })
        .count();
    if code == 0 && matches!(commit_type, "feat" | "fix") {
        report.warning(
            "no_code_type_mismatch",
            format!("Type mismatch: no code files changed but type is \"{commit_type}\""),
        );
    }
    report
}

fn type_scope_consistency(commit_type: &str, stat: &str, body: &[String]) -> ValidationReport {
    let mut report = ValidationReport::default();
    let paths = stat_paths(stat);
    // The leading "/" lets needles such as "/docs/" match root-level directories.
    let lower_paths: Vec<String> = paths
        .iter()
        .map(|path| format!("/{}", path.to_lowercase()))
        .collect();
    let any_path = |needles: &[&str]| {
        lower_paths
            .iter()
            .any(|path| needles.iter().any(|needle| path.contains(needle)))
    };
    let warn = |report: &mut ValidationReport, code: &'static str, message: &str| {
        report.warning(code, format!("Commit type '{commit_type}' {message}"));
    };
    match commit_type {
        "docs" => {
            let has_docs = paths.iter().any(|path| {
                extension_of(path).is_some_and(|ext| TABLES.doc_extensions.contains(&ext))
            }) || any_path(&["/docs/", "readme"]);
            if !has_docs {
                warn(
                    &mut report,
                    "docs_without_docs",
                    "but no documentation files changed",
                );
            }
        }
        "test" => {
            if !any_path(&["/test", "_test.", ".test."]) {
                warn(
                    &mut report,
                    "test_without_tests",
                    "but no test files changed",
                );
            }
        }
        "style" => {
            if paths.iter().any(|path| {
                extension_of(path).is_some_and(|ext| TABLES.code_extensions.contains(&ext))
            }) {
                warn(&mut report, "style_with_code", "but code files changed");
            }
        }
        "ci" => {
            if !any_path(&[".github/workflows", ".gitlab-ci", "jenkinsfile"]) {
                warn(
                    &mut report,
                    "ci_without_ci",
                    "but no CI configuration files changed",
                );
            }
        }
        "build" => {
            if !any_path(&["cargo.toml", "package.json", "makefile", "build."]) {
                warn(
                    &mut report,
                    "build_without_build",
                    "but no build files changed",
                );
            }
        }
        "refactor" => {
            let created = stat.lines().any(|line| {
                let trimmed = line.trim();
                trimmed.starts_with("create mode") || trimmed.to_lowercase().contains("new file")
            });
            if created {
                warn(
                    &mut report,
                    "refactor_with_new_files",
                    "but new files were created; verify no new capabilities were added",
                );
            }
        }
        "perf" => {
            let body_text = body.join(" ").to_lowercase();
            let evidenced = ["faster", "optimization", "performance", "optimized"]
                .iter()
                .any(|word| body_text.contains(word));
            if !any_path(&["bench", "perf", "profile"]) && !evidenced {
                warn(
                    &mut report,
                    "perf_without_evidence",
                    "but no performance files or optimization keywords were found",
                );
            }
        }
        _ => {}
    }
    report
}

//! Deterministic proposal used when the agent fails or never submits one.

use super::state::{CommitProposal, ConventionalAnalysis, ConventionalDetail, NumstatEntry};

const TEST_PATTERNS: [&str; 7] = [
    "/test/",
    "/tests/",
    "/__tests__/",
    "_test.",
    ".test.",
    ".spec.",
    "_spec.",
];
const DOC_EXTENSIONS: [&str; 4] = [".md", ".txt", ".rst", ".adoc"];
const CONFIG_EXTENSIONS: [&str; 7] = [".json", ".yaml", ".yml", ".toml", ".xml", ".ini", ".cfg"];
const STYLE_EXTENSIONS: [&str; 4] = [".css", ".scss", ".less", ".sass"];

/// The warning recorded on every fallback proposal.
pub(crate) const FALLBACK_WARNING: &str = "Commit generated using fallback due to agent failure";

/// Build the full fallback proposal: analysis plus summary plus warning.
pub(crate) fn generate_fallback_proposal(numstat: &[NumstatEntry]) -> CommitProposal {
    let analysis = generate_fallback_analysis(numstat);
    let summary = generate_fallback_summary(&analysis.commit_type, numstat);
    CommitProposal {
        analysis,
        summary,
        warnings: vec![FALLBACK_WARNING.to_owned()],
    }
}

/// Infer a type from file paths and list the first three files as details.
pub(crate) fn generate_fallback_analysis(numstat: &[NumstatEntry]) -> ConventionalAnalysis {
    let commit_type = infer_type_from_files(numstat);
    let details = numstat
        .iter()
        .take(3)
        .map(|entry| ConventionalDetail {
            text: format!("Updated {}", basename(&entry.path)),
            changelog_category: None,
            user_visible: Some(false),
        })
        .collect();
    ConventionalAnalysis {
        commit_type: commit_type.to_owned(),
        scope: None,
        details,
        issue_refs: Vec::new(),
    }
}

/// Verb-phrase summary such as `updated tests for lib.rs and 2 others`.
pub(crate) fn generate_fallback_summary(commit_type: &str, numstat: &[NumstatEntry]) -> String {
    let verb = match commit_type {
        "test" => "updated tests for",
        "docs" => "updated documentation for",
        "refactor" => "refactored",
        "style" => "formatted",
        "revert" => "reverted changes in",
        _ => "updated",
    };
    let file = numstat
        .first()
        .map_or("files", |entry| basename(&entry.path));
    match numstat.len() {
        0 | 1 => format!("{verb} {file}"),
        2 => format!("{verb} {file} and 1 other"),
        n => format!("{verb} {file} and {} others", n - 1),
    }
}

fn infer_type_from_files(numstat: &[NumstatEntry]) -> &'static str {
    if numstat.is_empty() {
        return "chore";
    }
    let (mut has_tests, mut has_docs, mut has_config, mut has_style, mut has_source) =
        (false, false, false, false, false);
    for entry in numstat {
        let lower = entry.path.to_lowercase();
        let ext = extension(&entry.path);
        let anchored = format!("/{lower}");
        if TEST_PATTERNS
            .iter()
            .any(|pattern| anchored.contains(pattern))
        {
            has_tests = true;
        } else if DOC_EXTENSIONS.contains(&ext.as_str()) {
            has_docs = true;
        } else if CONFIG_EXTENSIONS.contains(&ext.as_str()) {
            has_config = true;
        } else if STYLE_EXTENSIONS.contains(&ext.as_str()) {
            has_style = true;
        } else {
            has_source = true;
        }
    }
    if has_tests && !has_source && !has_docs {
        return "test";
    }
    if has_docs && !has_source && !has_tests {
        return "docs";
    }
    if has_style && !has_source && !has_tests {
        return "style";
    }
    if has_config && !has_source && !has_tests && !has_docs {
        return "chore";
    }
    "refactor"
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Lowercase extension including the dot, or empty when none. Mirrors `path.extname` on the basename.
fn extension(path: &str) -> String {
    let name = basename(path);
    match name.rfind('.') {
        Some(index) => name[index..].to_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests;

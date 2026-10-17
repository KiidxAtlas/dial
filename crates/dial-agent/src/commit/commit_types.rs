//! Conventional commit types, scope normalization, and the shared commit data model.

use std::sync::LazyLock;

use serde::Deserialize;

/// Canonical commit types in their fixed order.
pub(crate) const COMMIT_TYPE_ORDER: [&str; 22] = [
    "feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci", "revert",
    "deps", "security", "config", "ux", "release", "hotfix", "infra", "init", "merge", "hack",
    "wip",
];

/// Fallback type when a model names an unknown one.
pub(crate) const DEFAULT_COMMIT_TYPE: &str = "chore";

#[derive(Deserialize)]
struct TypesFile {
    classifier_hint: String,
    types: Vec<TypeEntry>,
}

#[derive(Deserialize)]
struct TypeEntry {
    name: String,
    description: String,
    #[serde(default)]
    hint: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

static TYPES_FILE: LazyLock<TypesFile> = LazyLock::new(|| {
    serde_json::from_str(include_str!("resources/commit_types.json"))
        .unwrap_or_else(|error| panic!("commit_types.json is invalid: {error}"))
});

/// Lowercase alias → canonical type name.
static ALIASES: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    TYPES_FILE
        .types
        .iter()
        .flat_map(|entry| {
            entry
                .aliases
                .iter()
                .map(|alias| (alias.trim().to_lowercase(), entry.name.clone()))
                .collect::<Vec<_>>()
        })
        .collect()
});

/// A finished conventional commit message before formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMessage {
    pub commit_type: String,
    pub scope: Option<String>,
    pub summary: String,
    pub body: Vec<String>,
    pub footers: Vec<String>,
}

/// Analysis of a diff produced by the model before the summary is finalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConventionalAnalysis {
    pub commit_type: String,
    pub scope: Option<String>,
    pub summary: Option<String>,
    pub details: Vec<String>,
    pub issue_refs: Vec<String>,
}

/// Canonical type for `raw`, honoring aliases; `None` when unknown.
pub(crate) fn canonical_commit_type(raw: &str) -> Option<String> {
    let lowered = raw.trim().to_lowercase();
    if COMMIT_TYPE_ORDER.contains(&lowered.as_str()) {
        return Some(lowered);
    }
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == lowered)
        .map(|(_, name)| name.clone())
}

/// Canonical type for `raw`, falling back to `chore`.
pub(crate) fn coerce_commit_type(raw: &str) -> String {
    canonical_commit_type(raw).unwrap_or_else(|| DEFAULT_COMMIT_TYPE.to_owned())
}

/// Normalize a scope to lowercase `segment/segment` form (at most two segments); `None` when empty or a placeholder.
pub(crate) fn coerce_optional_scope(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_lowercase();
    if matches!(lowered.as_str(), "null" | "none" | "n/a") {
        return None;
    }
    let normalized = lowered.replace('\\', "/");
    let segments: Vec<String> = normalized
        .split('/')
        .map(sanitize_scope_segment)
        .filter(|segment| !segment.is_empty())
        .take(2)
        .collect();
    if segments.is_empty() {
        None
    } else {
        Some(segments.join("/"))
    }
}

fn sanitize_scope_segment(segment: &str) -> String {
    let mut out = String::new();
    for ch in segment.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '_' {
            if !out.is_empty() && !out.ends_with(['-', '_']) {
                out.push(ch);
            }
        } else if (ch.is_whitespace() || ch == '.') && !out.is_empty() && !out.ends_with(['-', '_'])
        {
            out.push('-');
        }
    }
    out.trim_matches(['-', '_']).to_owned()
}

/// Human-readable list of the allowed types for prompts.
pub(crate) fn types_description() -> String {
    let mut lines: Vec<String> = TYPES_FILE
        .types
        .iter()
        .map(|entry| {
            match entry
                .hint
                .as_deref()
                .map(str::trim)
                .filter(|hint| !hint.is_empty())
            {
                Some(hint) => format!("- {}: {} ({hint})", entry.name, entry.description),
                None => format!("- {}: {}", entry.name, entry.description),
            }
        })
        .collect();
    let hint = TYPES_FILE.classifier_hint.trim();
    if !hint.is_empty() {
        lines.push(hint.to_owned());
    }
    lines.join("\n")
}

/// Format `type(scope): summary`, then body bullets and footers.
pub fn format_conventional_commit(message: &CommitMessage) -> String {
    let header = match &message.scope {
        Some(scope) => format!("{}({scope}): {}", message.commit_type, message.summary),
        None => format!("{}: {}", message.commit_type, message.summary),
    };
    let mut out = header;
    if !message.body.is_empty() {
        out.push_str("\n\n");
        out.push_str(
            &message
                .body
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !message.footers.is_empty() {
        out.push_str("\n\n");
        out.push_str(&message.footers.join("\n"));
    }
    out
}

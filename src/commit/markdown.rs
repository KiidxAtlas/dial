//! Lenient parsers for model output written as markdown, XML-ish tags, or JSON.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::{
    commit_types::{
        CommitMessage, ConventionalAnalysis, DEFAULT_COMMIT_TYPE, canonical_commit_type,
        coerce_commit_type, coerce_optional_scope,
    },
    text::{
        collapse_ws, cp_len, dedupe_ci, ensure_sentence, first_word, strip_wrapping_quotes,
        take_chars,
    },
};

static PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:#+\s*)?(?P<type>[a-z][a-z0-9-]*)(?:\((?P<scope>[^)]+)\))?!?\s*:\s*(?P<summary>.+?)\s*$")
        .expect("valid regex")
});
static ISSUE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"#[0-9]+(?:\s*-\s*#?[0-9]+)?").expect("valid regex"));
static FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)```(?:json|markdown|md)?\s*(.*?)```").expect("valid regex"));
static TYPED_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^([a-z][a-z0-9-]*)(?:\(([^)]*)\))?:\s+(.*)$").expect("valid regex")
});
static HEADING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:#+\s*)?(?:file\s*[:=-]\s*)?`?([^`]+?)`?\s*:?$").expect("valid regex")
});
static NUMBERED_BULLET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[0-9]+[.)]\s+(.+)$").expect("valid regex"));

const TRAILING_SEPARATORS: &[char] = &[' ', ',', ';', ':', '-'];

/// One file's observations parsed from map-phase output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileObservations {
    pub path: String,
    pub observations: Vec<String>,
}

/// A `type(scope): summary` heading with its type already resolved.
struct Heading {
    commit_type: String,
    scope: Option<String>,
    summary: String,
}

/// A `type(scope): summary` heading before the type is validated.
struct RawHeading {
    kind: String,
    scope: Option<String>,
    summary: String,
}

fn summary_verb(commit_type: &str) -> &'static str {
    match commit_type {
        "feat" => "added",
        "fix" => "fixed",
        "refactor" => "restructured",
        "docs" => "documented",
        "test" => "tested",
        "perf" => "optimized",
        "build" | "ci" | "chore" => "updated",
        "style" => "formatted",
        "revert" => "reverted",
        _ => "changed",
    }
}

fn safe_summary(commit_type: &str) -> &'static str {
    match commit_type {
        "refactor" => "restructured change",
        "feat" => "added functionality",
        "fix" => "fixed issue",
        "docs" => "documented updates",
        "test" => "tested changes",
        "chore" | "build" | "ci" | "style" => "updated tooling",
        "perf" => "optimized performance",
        "revert" => "reverted previous commit",
        _ => "updated files",
    }
}

/// Remove a conventional type/scope prefix from model summary text.
pub(crate) fn strip_type_prefix(
    text: &str,
    commit_type: Option<&str>,
    scope: Option<&str>,
) -> String {
    if let Some(commit_type) = commit_type {
        let trimmed = text.trim();
        let mut prefixes = Vec::new();
        match scope.filter(|scope| !scope.is_empty()) {
            Some(scope) => {
                prefixes.push(format!("{commit_type}({scope}): "));
                prefixes.push(format!("{commit_type}: "));
            }
            None => prefixes.push(format!("{commit_type}: ")),
        }
        for prefix in &prefixes {
            if let Some(head) = trimmed.get(..prefix.len())
                && head.to_lowercase() == prefix.to_lowercase()
            {
                return trimmed[prefix.len()..].trim().to_owned();
            }
        }
        if let Some(parsed) = TYPED_PREFIX_RE.captures(trimmed)
            && parsed.get(1).map(|m| m.as_str().to_lowercase()) == Some(commit_type.to_lowercase())
        {
            return parsed
                .get(3)
                .map(|m| m.as_str().trim().to_owned())
                .unwrap_or_default();
        }
        return trimmed.to_owned();
    }
    let cleaned = clean_markdown_text(text);
    let first_line = cleaned.lines().next().unwrap_or("").trim();
    let summary = PREFIX_RE
        .captures(first_line)
        .and_then(|caps| caps.name("summary"))
        .map_or(first_line, |m| m.as_str());
    strip_trailing_period_owned(strip_wrapping_quotes(summary.trim()))
}

/// Deterministic, type-aware summary used when the model gives none.
pub(crate) fn fallback_summary(
    stat: &str,
    details: &[String],
    diff: &str,
    limit: Option<usize>,
    commit_type: &str,
) -> String {
    let commit_type =
        canonical_commit_type(commit_type).unwrap_or_else(|| DEFAULT_COMMIT_TYPE.to_owned());
    let mut candidate = String::new();
    let mut needs_verb = false;
    for detail in details {
        let cleaned_detail = detail
            .trim_start_matches(|c: char| {
                matches!(c, '-' | '*' | '•' | '–' | '+') || c.is_whitespace()
            })
            .trim();
        candidate = strip_type_prefix(cleaned_detail, None, None);
        if candidate.is_empty() {
            continue;
        }
        let (text, verb_needed) = strip_leading_type_word(&candidate, &commit_type);
        candidate = text;
        needs_verb = verb_needed;
        break;
    }
    if candidate.is_empty() {
        let area = primary_stat_subject(stat).or_else(|| primary_stat_subject(diff));
        candidate = match area {
            Some(area) if area.to_lowercase() != "files" => format!("Updated {area}"),
            _ => "Updated files".to_owned(),
        };
    }
    let collapsed = collapse_ws(&candidate);
    let trimmed = collapsed.trim().trim_end_matches(['.', ';', ':']).trim();
    candidate = if trimmed.is_empty() {
        "Updated files".to_owned()
    } else {
        trimmed.to_owned()
    };
    if needs_verb && !starts_with_past_tense(&candidate) {
        candidate = format!("{} {candidate}", summary_verb(&commit_type));
    }
    let limit = limit.unwrap_or(72).max(1);
    candidate = truncate_summary(&candidate, limit)
        .trim_end_matches('.')
        .to_owned();
    if first_word(&candidate).to_lowercase() == commit_type {
        candidate = safe_summary(&commit_type).to_owned();
    }
    truncate_summary(&candidate, limit)
}

/// Parse summary output from markdown, XML-ish tags, JSON, or plain text.
pub(crate) fn parse_summary_markdown(text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Ok(String::new());
    }
    if let Some(Value::Object(map)) = try_json(text) {
        for key in ["summary", "title", "message"] {
            if let Some(Value::String(value)) = map.get(key)
                && !value.trim().is_empty()
            {
                return Ok(strip_type_prefix(value, None, None));
            }
        }
    }
    let cleaned = clean_markdown_text(text);
    let mut summary = extract_tag_lenient(&cleaned, "summary").unwrap_or(cleaned);
    summary = strip_heading_markers(&summary);
    summary = strip_label_prefix(&summary);
    summary = strip_wrapping_quotes(&summary).to_owned();
    summary = collapse_ws(&summary);
    if summary.is_empty() {
        return Err("Markdown summary empty after normalization".to_owned());
    }
    Ok(strip_type_prefix(&summary, None, None))
}

/// Parse a conventional analysis from the lenient markdown contract.
pub(crate) fn parse_conventional_analysis_markdown(
    text: &str,
    default_type: &str,
) -> Result<ConventionalAnalysis, String> {
    if let Some(Value::Object(map)) = try_json(text) {
        return analysis_from_mapping(&map, default_type);
    }
    let cleaned = clean_markdown_text(text);
    let lines: Vec<&str> = cleaned.lines().collect();
    let mut heading: Option<(usize, Heading)> = None;
    let mut coerced: Option<(usize, Heading)> = None;
    for (index, line) in lines.iter().enumerate().take(5) {
        let candidate = strip_heading_markers(line);
        if let Some(parsed) = parse_heading_line(&candidate, false) {
            heading = Some((index, parsed));
            break;
        }
        if coerced.is_none()
            && line.trim().starts_with('#')
            && let Some(parsed) = parse_heading_line(&candidate, true)
        {
            coerced = Some((index, parsed));
        }
    }
    let (index, heading) = heading
        .or(coerced)
        .ok_or_else(|| "Markdown analysis type(scope): summary heading not found".to_owned())?;

    let mut details = Vec::new();
    let mut issue_refs = Vec::new();
    for line in &lines[index + 1..] {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let lower = stripped.to_lowercase();
        if ["fixes:", "closes:", "resolves:"]
            .iter()
            .any(|prefix| lower.starts_with(*prefix))
        {
            if let Some(separator) = stripped.find(':') {
                for reference in stripped[separator + 1..].split(',') {
                    let reference = reference.trim();
                    if !reference.is_empty() {
                        issue_refs.push(reference.to_owned());
                    }
                }
            }
            continue;
        }
        let Some(bullet) = strip_bullet(stripped) else {
            continue;
        };
        details.push(ensure_sentence(&bullet));
        issue_refs.extend(ISSUE_RE.find_iter(&bullet).map(|m| m.as_str().to_owned()));
    }
    Ok(ConventionalAnalysis {
        commit_type: heading.commit_type,
        scope: coerce_optional_scope(heading.scope.as_deref()),
        summary: Some(heading.summary),
        details: dedupe_ci(details),
        issue_refs: dedupe_ci(issue_refs),
    })
}

/// Parse a complete conventional commit from markdown or JSON text.
pub(crate) fn parse_fast_commit_markdown(
    text: &str,
    default_type: &str,
) -> Result<CommitMessage, String> {
    let analysis = match try_json(text) {
        Some(Value::Object(map)) => analysis_from_mapping(&map, default_type)?,
        _ => parse_conventional_analysis_markdown(text, default_type)?,
    };
    let summary = analysis
        .summary
        .clone()
        .unwrap_or_else(|| fallback_summary("", &analysis.details, "", None, DEFAULT_COMMIT_TYPE));
    if summary.trim().is_empty() {
        return Err("Commit summary cannot be empty".to_owned());
    }
    Ok(CommitMessage {
        commit_type: analysis.commit_type,
        scope: analysis.scope,
        summary,
        body: analysis.details,
        footers: Vec::new(),
    })
}

/// Coerce a JSON object into a conventional analysis.
fn analysis_from_mapping(
    payload: &Map<String, Value>,
    default_type: &str,
) -> Result<ConventionalAnalysis, String> {
    let type_text = first_present(payload, &["type", "commit_type"])
        .map(value_to_string)
        .unwrap_or_default();
    let type_text = match type_text.trim() {
        "" => default_type.to_owned(),
        trimmed => trimmed.to_owned(),
    };
    let commit_type = canonical_commit_type(&type_text)
        .ok_or_else(|| format!("Invalid commit type: {type_text}"))?;

    let scope_text = match payload.get("scope") {
        None | Some(Value::Null) => String::new(),
        Some(value) => value_to_string(value).trim().to_owned(),
    };
    let scope = match scope_text.to_lowercase().as_str() {
        "" | "null" | "none" | "(none)" => None,
        _ => Some(scope_text),
    };

    let summary_text = first_present(payload, &["summary"])
        .map(value_to_string)
        .unwrap_or_default();
    let summary =
        Some(strip_type_prefix(&summary_text, None, None)).filter(|summary| !summary.is_empty());

    let raw_details = first_present(payload, &["details", "body"])
        .cloned()
        .unwrap_or(Value::Null);
    let mut details = Vec::new();
    for item in iterable(&raw_details) {
        let text = match &item {
            Value::Object(map) => first_present(map, &["text", "summary", "detail"])
                .map(value_to_string)
                .unwrap_or_default(),
            other => value_to_string(other),
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        details.push(ensure_sentence(text));
    }

    let raw_refs = first_present(payload, &["issue_refs", "issues"])
        .cloned()
        .unwrap_or(Value::Null);
    let refs: Vec<String> = iterable(&raw_refs)
        .iter()
        .map(|item| value_to_string(item).trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect();
    let issue_refs = strings_from(&Value::Array(refs.into_iter().map(Value::String).collect()));

    Ok(ConventionalAnalysis {
        commit_type,
        scope: coerce_optional_scope(scope.as_deref()),
        summary,
        details,
        issue_refs,
    })
}

/// Parse map-phase file observations from JSON or lenient markdown.
pub(crate) fn parse_file_observations_markdown(text: &str) -> Vec<FileObservations> {
    let payload = try_json(text);
    if let Some(Value::Object(map)) = &payload
        && let Some(Value::Array(files)) = map.get("files")
    {
        return files
            .iter()
            .filter_map(Value::as_object)
            .map(coerce_file_observations)
            .collect();
    }
    if let Some(Value::Array(items)) = &payload {
        return items
            .iter()
            .filter_map(Value::as_object)
            .map(coerce_file_observations)
            .collect();
    }

    let cleaned = clean_markdown_text(text);
    let mut files = Vec::new();
    let mut current_path: Option<String> = None;
    let mut observations: Vec<String> = Vec::new();
    for line in cleaned.lines() {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let candidate = HEADING_RE
            .captures(stripped)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_owned())
            .unwrap_or_default();
        let bullet = strip_bullet(stripped);
        if bullet.is_none()
            && !candidate.is_empty()
            && (candidate.contains('/') || candidate.contains('.') || stripped.starts_with('#'))
        {
            if let Some(path) = current_path.take() {
                files.push(FileObservations {
                    path,
                    observations: std::mem::take(&mut observations),
                });
            }
            current_path = Some(candidate.trim().to_owned());
        } else if let (Some(bullet), Some(_)) = (bullet, current_path.as_ref()) {
            observations.push(strip_trailing_period_owned(&bullet));
        }
    }
    if let Some(path) = current_path {
        files.push(FileObservations { path, observations });
    }
    files
}

fn coerce_file_observations(item: &Map<String, Value>) -> FileObservations {
    FileObservations {
        path: first_present(item, &["path", "file"])
            .map(value_to_string)
            .unwrap_or_default(),
        observations: observation_strings(
            first_present(item, &["observations", "details"]).unwrap_or(&Value::Null),
        ),
    }
}

fn observation_strings(value: &Value) -> Vec<String> {
    if let Value::String(text) = value {
        let stripped = text.trim();
        if stripped.starts_with('[')
            && let Ok(Value::Array(items)) = serde_json::from_str::<Value>(stripped)
        {
            return items
                .iter()
                .map(|item| value_to_string(item).trim().to_owned())
                .filter(|item| !item.is_empty())
                .collect();
        }
        return text
            .lines()
            .map(|line| {
                line.trim_start_matches(|c: char| matches!(c, '-' | '*' | '•') || c.is_whitespace())
                    .trim()
                    .to_owned()
            })
            .filter(|line| !line.is_empty())
            .collect();
    }
    iterable(value)
        .iter()
        .map(|item| value_to_string(item).trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Leading word is a type name (or its past-tense form): drop it and mark that a verb is needed.
fn strip_leading_type_word(text: &str, commit_type: &str) -> (String, bool) {
    let cleaned = text.trim().trim_end_matches('.');
    let mut variants = [
        commit_type.to_owned(),
        format!("{commit_type}ed"),
        format!("{commit_type}d"),
    ];
    variants.sort_by_key(|variant| std::cmp::Reverse(variant.len()));
    for variant in &variants {
        if let Some(head) = cleaned.get(..variant.len())
            && head.to_lowercase() == variant.to_lowercase()
            && cleaned[variant.len()..].starts_with(' ')
        {
            return (cleaned[variant.len()..].trim().to_owned(), true);
        }
    }
    (cleaned.to_owned(), false)
}

fn starts_with_past_tense(text: &str) -> bool {
    let first = first_word(text).to_lowercase();
    first.ends_with("ed")
        || matches!(
            first.as_str(),
            "built" | "changed" | "documented" | "fixed" | "optimized" | "restructured" | "updated"
        )
}

/// First non-empty line of a diff stat, up to its first `|`; `files` when that part is empty.
fn primary_stat_subject(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let part = line.split('|').next().unwrap_or("").trim();
    Some(if part.is_empty() {
        "files".to_owned()
    } else {
        part.to_owned()
    })
}

/// Parse the first JSON object or array found in `text` (fenced, embedded, or bare).
fn try_json(text: &str) -> Option<Value> {
    let cleaned_owned = clean_markdown_text(text);
    let cleaned = cleaned_owned.trim();
    let fenced = FENCE_RE
        .captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().trim());
    // Unfenced text is also tried unnormalized: `normalize_escaped_whitespace` turns JSON's `\n`
    // string escapes into raw control characters, which serde rejects.
    let sources: Vec<&str> = match fenced {
        Some(body) => vec![body, cleaned],
        None => vec![cleaned, text.trim()],
    };
    sources.into_iter().find_map(parse_json_span)
}

/// Parse the outermost `{...}` or `[...]` span of `text`, preferring whichever opens first.
fn parse_json_span(text: &str) -> Option<Value> {
    let mut spans: Vec<(usize, &str)> = Vec::new();
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(start), Some(end)) = (text.find(open), text.rfind(close))
            && end >= start
        {
            spans.push((start, &text[start..=end]));
        }
    }
    spans.sort_by_key(|(start, _)| *start);
    spans
        .into_iter()
        .map(|(_, span)| span)
        .chain(std::iter::once(text))
        .filter(|candidate| candidate.starts_with('[') || candidate.starts_with('{'))
        .find_map(|candidate| serde_json::from_str::<Value>(candidate).ok())
}

/// Strip code fences and normalize escaped whitespace outside inline code.
fn clean_markdown_text(text: &str) -> String {
    let normalized = normalize_escaped_whitespace(text.trim());
    let cleaned = if let Some(after_open) = normalized.strip_prefix("```") {
        match after_open.find('\n') {
            Some(content_start) => {
                let body = &after_open[content_start + 1..];
                match body.rfind("```") {
                    Some(end) => body[..end].trim().to_owned(),
                    None => body.trim().to_owned(),
                }
            }
            None => normalized.clone(),
        }
    } else {
        normalized
            .lines()
            .filter(|line| !line.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned()
    };
    cleaned.replace("\r\n", "\n")
}

/// Turn literal `\n`, `\r`, `\t` escapes into whitespace, leaving inline code spans alone.
fn normalize_escaped_whitespace(text: &str) -> String {
    if !text.contains('\\') {
        return text.to_owned();
    }
    text.split('`')
        .enumerate()
        .map(|(index, part)| {
            if index % 2 == 0 {
                part.replace("\\r\\n", "\n")
                    .replace("\\n", "\n")
                    .replace("\\r", "\n")
                    .replace("\\t", "\t")
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("`")
}

/// Body of `<tag ...>body</...>`, tolerating a missing close tag.
fn extract_tag_lenient(text: &str, tag: &str) -> Option<String> {
    let open = text.to_ascii_lowercase().find(&format!("<{tag}"))?;
    let open_end = text[open..].find('>')? + open;
    let rest = &text[open_end + 1..];
    let body = match rest.find("</") {
        Some(close) => &rest[..close],
        None => rest,
    };
    Some(body.trim().to_owned())
}

fn strip_label_prefix(text: &str) -> String {
    let stripped = text.trim();
    let Some(separator) = stripped.find(':') else {
        return stripped.to_owned();
    };
    let label = stripped[..separator].trim().to_lowercase();
    if matches!(
        label.as_str(),
        "title" | "summary" | "description" | "result"
    ) {
        stripped[separator + 1..].trim().to_owned()
    } else {
        stripped.to_owned()
    }
}

/// Drop a leading `#` heading marker and one matching emphasis wrapper.
fn strip_heading_markers(text: &str) -> String {
    let mut stripped = text.trim().trim_start_matches('#').trim().to_owned();
    for marker in ["**", "*", "__", "_"] {
        if stripped.len() > marker.len() * 2
            && stripped.starts_with(marker)
            && stripped.ends_with(marker)
        {
            stripped = stripped[marker.len()..stripped.len() - marker.len()]
                .trim()
                .to_owned();
        }
    }
    stripped
}

fn parse_heading_line(line: &str, coerce: bool) -> Option<Heading> {
    let split = split_heading(line)?;
    if let Some(commit_type) = canonical_commit_type(&split.kind) {
        return Some(Heading {
            commit_type,
            scope: split.scope,
            summary: split.summary,
        });
    }
    if coerce && is_plain_type_word(&split.kind) && !split.summary.starts_with(['"', '{', '[']) {
        return Some(Heading {
            commit_type: coerce_commit_type(&split.kind),
            scope: split.scope,
            summary: split.summary,
        });
    }
    None
}

fn is_plain_type_word(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphabetic() || c == '-')
}

/// Split `type(scope): summary` at the first colon; `None` when a part is missing.
fn split_heading(line: &str) -> Option<RawHeading> {
    let separator = line.find(':')?;
    let kind = line[..separator].trim();
    let summary = line[separator + 1..].trim();
    if kind.is_empty() || summary.is_empty() {
        return None;
    }
    let (kind, scope) = match kind.find('(') {
        Some(start) => {
            let end = kind[start + 1..].find(')')? + start + 1;
            let scope = kind[start + 1..end].trim();
            let kind = kind[..start].trim();
            (kind, (!scope.is_empty()).then(|| scope.to_owned()))
        }
        None => (kind, None),
    };
    if kind.is_empty() {
        return None;
    }
    Some(RawHeading {
        kind: kind.to_owned(),
        scope,
        summary: summary.to_owned(),
    })
}

/// Trim, remove one trailing period, and trim again.
fn strip_trailing_period_owned(text: &str) -> String {
    match text.trim_end().strip_suffix('.') {
        Some(rest) => rest.trim_end().to_owned(),
        None => text.trim().to_owned(),
    }
}

/// Trim, remove trailing separators, then cut at the last space within `limit` code points.
fn truncate_summary(text: &str, limit: usize) -> String {
    let cleaned = strip_trailing_period_owned(text.trim());
    if cp_len(&cleaned) <= limit {
        return cleaned;
    }
    let sliced = take_chars(&cleaned, limit.max(1));
    let head = match sliced.rfind(' ') {
        Some(at) => &sliced[..at],
        None => sliced,
    };
    let trimmed = head.trim_end_matches(TRAILING_SEPARATORS);
    if trimmed.is_empty() {
        take_chars(&cleaned, limit)
            .trim_end_matches(TRAILING_SEPARATORS)
            .to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Strip a leading bullet glyph or numbered marker; `None` when nothing remains.
fn strip_bullet(line: &str) -> Option<String> {
    let stripped = line.trim_start();
    for glyph in ["- ", "* ", "• ", "– ", "+ "] {
        if let Some(rest) = stripped.strip_prefix(glyph) {
            return Some(rest.trim().to_owned()).filter(|text| !text.is_empty());
        }
    }
    NUMBERED_BULLET_RE
        .captures(stripped)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// First non-null value among `keys` (JS `??` chain).
fn first_present<'a>(map: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| map.get(*key))
        .find(|value| !value.is_null())
}

/// Items of a JSON value treated as a list (JS `iterable`).
fn iterable(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::String(text) if text.trim().is_empty() => Vec::new(),
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    }
}

/// Flatten a JSON value into display strings (JS `stringsFrom`).
fn strings_from(value: &Value) -> Vec<String> {
    match value {
        Value::Null => Vec::new(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.starts_with('[')
                && let Ok(decoded) = serde_json::from_str::<Value>(trimmed)
            {
                return strings_from(&decoded);
            }
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        }
        Value::Array(items) => items.iter().flat_map(strings_from).collect(),
        Value::Object(map) => map
            .iter()
            .flat_map(|(key, inner)| {
                let inner_values = strings_from(inner);
                if inner_values.is_empty() {
                    vec![key.clone()]
                } else {
                    inner_values
                        .into_iter()
                        .map(|item| format!("{key}: {item}"))
                        .collect()
                }
            })
            .collect(),
        other => vec![other.to_string()],
    }
}

/// String form of a JSON value (JS `String(value)`); strings unquoted.
fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

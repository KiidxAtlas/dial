//! Contains third-party MIT-licensed code; see THIRD_PARTY_NOTICES.
//! Pure string algorithms only; Dial owns path authority, witnesses and filesystem transactions.
use super::error::EditError;
use regex::Regex;
use std::{collections::BTreeSet, sync::LazyLock};
const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";
const MULTI_FILE_MARKERS: [&str; 4] = [
    "*** Update File:",
    "*** Add File:",
    "*** Delete File:",
    "diff --git ",
];
const DIFF_METADATA_PREFIXES: [&str; 15] = [
    "*** Update File:",
    "*** Add File:",
    "*** Delete File:",
    "diff --git ",
    "index ",
    "--- ",
    "+++ ",
    "new file mode ",
    "deleted file mode ",
    "rename from ",
    "rename to ",
    "similarity index ",
    "dissimilarity index ",
    "old mode ",
    "new mode ",
];
const PATCH_WRAPPER_PREFIXES: [&str; 2] = ["*** Begin Patch", "*** End Patch"];
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffHunk {
    /// Optional textual or symbolic hunk anchor.
    pub change_context: Option<String>,
    /// Optional 1-indexed old-file line hint.
    pub old_start_line: Option<u32>,
    /// Optional 1-indexed new-file line hint.
    pub new_start_line: Option<u32>,
    /// Whether the hunk contains unchanged context.
    pub has_context_lines: bool,
    /// Expected old-file lines.
    pub old_lines: Vec<String>,
    /// Replacement new-file lines.
    pub new_lines: Vec<String>,
    /// Whether the hunk carries an end-of-file marker.
    pub is_end_of_file: bool,
}

/// `--- ` / `+++ ` lines are unified file headers only as an adjacent pair followed by `@@`;
/// otherwise they are removed/added body content such as `-- x` or `++ i;`.
fn is_file_pair_prefix(line: &str) -> bool {
    line.starts_with("--- ") || line.starts_with("+++ ")
}

fn is_unified_file_header(lines: &[&str], index: usize) -> bool {
    let line = lines[index].trim();
    let following = |offset: usize| lines.get(index + offset).map(|next| next.trim());
    if line.starts_with("--- ") {
        following(1).is_some_and(|next| next.starts_with("+++ "))
            && following(2).is_some_and(|next| next.starts_with("@@"))
    } else if line.starts_with("+++ ") {
        following(1).is_some_and(|next| next.starts_with("@@"))
    } else {
        false
    }
}

fn is_diff_content_line(line: &str) -> bool {
    match line.as_bytes().first() {
        Some(b' ') => true,
        Some(b'+') => !line.starts_with("+++ "),
        Some(b'-') => !line.starts_with("--- "),
        _ => false,
    }
}

fn matches_trimmed_prefix(line: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| line.starts_with(prefix))
}

fn is_patch_wrapper_line(line: &str) -> bool {
    line == "***" || matches_trimmed_prefix(line, &PATCH_WRAPPER_PREFIXES)
}

/// Strip wrapper/metadata lines and trailing blank rows from a diff body.
pub fn normalize_diff(diff: &str) -> String {
    let mut lines = diff.split('\n').collect::<Vec<_>>();
    while let Some(last_line) = lines.last().copied() {
        if last_line.is_empty() || (last_line.trim().is_empty() && !is_diff_content_line(last_line))
        {
            lines.pop();
        } else {
            break;
        }
    }
    if lines
        .first()
        .is_some_and(|line| is_patch_wrapper_line(line.trim()))
    {
        lines.remove(0);
    }
    if lines
        .last()
        .is_some_and(|line| is_patch_wrapper_line(line.trim()))
    {
        lines.pop();
    }
    let kept = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            is_diff_content_line(line)
                || !matches_trimmed_prefix(line.trim(), &DIFF_METADATA_PREFIXES)
                || (is_file_pair_prefix(line.trim()) && !is_unified_file_header(&lines, *index))
        })
        .map(|(_, line)| *line)
        .collect::<Vec<_>>();
    kept.join("\n")
}

/// Strip a uniform `+` prefix from create-file content.
pub fn normalize_create_content(content: &str) -> String {
    let lines = content.split('\n').collect::<Vec<_>>();
    let non_empty_lines = lines
        .iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if !non_empty_lines.is_empty() && non_empty_lines.iter().all(|line| line.starts_with('+')) {
        return lines
            .into_iter()
            .map(|line| {
                line.strip_prefix("+ ")
                    .or_else(|| line.strip_prefix('+'))
                    .unwrap_or(line)
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    content.to_owned()
}

#[derive(Debug)]
struct UnifiedHunkHeader {
    old_start_line: u32,
    new_start_line: u32,
    change_context: Option<String>,
}

static UNIFIED_HUNK_HEADER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@\s*-(\d+)(?:,(\d+))?\s+\+(\d+)(?:,(\d+))?\s*@@(?:\s*(.*))?$")
        .expect("valid unified hunk header regex")
});
static LINE_HINT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^lines?\s+(\d+)(?:\s*-\s*(\d+))?(?:\s*@@)?$").expect("valid line hint regex")
});
static TOP_OF_FILE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(top|start|beginning)\s+of\s+file$").expect("valid top-of-file regex")
});
static NUMBERED_LINE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*?(\d{1,6})\s+?(.+)$").expect("valid numbered-line regex"));

fn parse_unified_hunk_header(line: &str) -> Option<UnifiedHunkHeader> {
    let captures = UNIFIED_HUNK_HEADER_REGEX.captures(line)?;
    let old_start_line = captures.get(1)?.as_str().parse().ok()?;
    let new_start_line = captures.get(3)?.as_str().parse().ok()?;
    let change_context = captures
        .get(5)
        .map(|value| value.as_str().trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Some(UnifiedHunkHeader {
        old_start_line,
        new_start_line,
        change_context,
    })
}

fn parse_error(message: impl AsRef<str>, line_number: u32) -> EditError {
    EditError::Parse {
        message: format!("Line {line_number}: {}", message.as_ref()),
        line: Some(line_number),
    }
}

fn strip_line_number_prefixes(hunk: &mut DiffHunk) {
    let all_lines = hunk
        .old_lines
        .iter()
        .chain(&hunk.new_lines)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if all_lines.len() < 2 {
        return;
    }
    let number_matches = all_lines
        .iter()
        .filter_map(|line| NUMBERED_LINE_REGEX.captures(line))
        .collect::<Vec<_>>();
    // Math.ceil(length * 0.6), without floating point.
    let minimum_matches = 2_usize.max((all_lines.len() * 6).div_ceil(10));
    if number_matches.len() < minimum_matches {
        return;
    }
    let numbers = number_matches
        .iter()
        .filter_map(|captures| captures.get(1)?.as_str().parse::<u32>().ok())
        .collect::<Vec<_>>();
    let sequential = numbers
        .windows(2)
        .filter(|pair| pair[1] == pair[0] + 1)
        .count();
    if numbers.len() >= 3 && sequential < 1_usize.max(numbers.len() - 2) {
        return;
    }
    let strip = |line: &String| {
        NUMBERED_LINE_REGEX
            .captures(line)
            .and_then(|captures| captures.get(2).map(|value| value.as_str().to_owned()))
            .unwrap_or_else(|| line.clone())
    };
    hunk.old_lines = hunk.old_lines.iter().map(strip).collect();
    hunk.new_lines = hunk.new_lines.iter().map(strip).collect();
}

struct ParseHunkResult {
    hunk: DiffHunk,
    lines_consumed: usize,
}

fn parse_one_hunk(
    lines: &[&str],
    line_number: u32,
    allow_missing_context: bool,
) -> Result<ParseHunkResult, EditError> {
    if lines.is_empty() {
        return Err(parse_error("Diff does not contain any lines", line_number));
    }
    let mut change_contexts = Vec::new();
    let mut old_start_line = None;
    let mut new_start_line = None;
    let mut start_index;
    let header_line = lines[0];
    let header_trimmed = header_line.trim_end();
    let is_header_line = header_line.starts_with("@@");
    let unified_header = is_header_line
        .then(|| parse_unified_hunk_header(header_trimmed))
        .flatten();
    let is_empty_context_marker = header_trimmed
        .strip_prefix("@@")
        .and_then(|rest| rest.strip_suffix("@@"))
        .is_some_and(|middle| middle.trim().is_empty());

    if is_header_line && (header_trimmed == EMPTY_CHANGE_CONTEXT_MARKER || is_empty_context_marker)
    {
        start_index = 1;
    } else if let Some(unified_header) = unified_header {
        if unified_header.old_start_line < 1 || unified_header.new_start_line < 1 {
            return Err(parse_error(
                "Line numbers in @@ header must be >= 1",
                line_number,
            ));
        }
        if let Some(change_context) = unified_header.change_context {
            change_contexts.push(change_context);
        }
        old_start_line = Some(unified_header.old_start_line);
        new_start_line = Some(unified_header.new_start_line);
        start_index = 1;
    } else if is_header_line && header_trimmed.starts_with(CHANGE_CONTEXT_MARKER) {
        let context_value = &header_trimmed[CHANGE_CONTEXT_MARKER.len()..];
        let trimmed_context_value = context_value.trim();
        let normalized_context_value = trimmed_context_value
            .strip_prefix("@@")
            .map_or(trimmed_context_value, str::trim_start);
        if let Some(captures) = LINE_HINT_REGEX.captures(normalized_context_value) {
            let value = captures
                .get(1)
                .expect("line hint capture")
                .as_str()
                .parse::<u32>()
                .unwrap_or(0);
            old_start_line = Some(value);
            new_start_line = Some(value);
            if value < 1 {
                return Err(parse_error("Line hint must be >= 1", line_number));
            }
        } else if TOP_OF_FILE_REGEX.is_match(normalized_context_value) {
            old_start_line = Some(1);
            new_start_line = Some(1);
        } else if !trimmed_context_value.is_empty() {
            change_contexts.push(context_value.to_owned());
        }
        start_index = 1;
    } else if is_header_line {
        let context_value = header_trimmed[2..].trim();
        if !context_value.is_empty() {
            change_contexts.push(context_value.to_owned());
        }
        start_index = 1;
    } else {
        if !allow_missing_context {
            return Err(parse_error(
                format!(
                    "Expected hunk to start with @@ context marker, got: '{}'",
                    lines[0]
                ),
                line_number,
            ));
        }
        start_index = 0;
    }

    if old_start_line.is_some_and(|value| value < 1) {
        return Err(parse_error(
            format!(
                "Line numbers must be >= 1 (got {})",
                old_start_line.unwrap_or_default()
            ),
            line_number,
        ));
    }
    if new_start_line.is_some_and(|value| value < 1) {
        return Err(parse_error(
            format!(
                "Line numbers must be >= 1 (got {})",
                new_start_line.unwrap_or_default()
            ),
            line_number,
        ));
    }

    while start_index < lines.len() {
        let next_line = lines[start_index];
        if !next_line.starts_with("@@") {
            break;
        }
        let trimmed = next_line.trim_end();
        if let Some(nested_context) = trimmed.strip_prefix(CHANGE_CONTEXT_MARKER) {
            if !nested_context.trim().is_empty() {
                change_contexts.push(nested_context.to_owned());
            }
            start_index += 1;
        } else if trimmed == EMPTY_CHANGE_CONTEXT_MARKER {
            start_index += 1;
        } else {
            break;
        }
    }
    if start_index >= lines.len() {
        return Err(parse_error(
            "Hunk does not contain any lines",
            line_number + 1,
        ));
    }

    let mut hunk = DiffHunk {
        change_context: (!change_contexts.is_empty()).then(|| change_contexts.join("\n")),
        old_start_line,
        new_start_line,
        ..DiffHunk::default()
    };
    let mut parsed_lines = 0_usize;
    for index in start_index..lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        let next_line = lines.get(index + 1).copied();
        if line.is_empty()
            && parsed_lines > 0
            && next_line.is_some_and(|next| next.trim_start().starts_with("@@"))
        {
            break;
        }
        if !is_diff_content_line(line)
            && line.trim_end() == EOF_MARKER
            && line.starts_with(EOF_MARKER)
        {
            if parsed_lines == 0 {
                return Err(parse_error(
                    "Hunk does not contain any lines",
                    line_number + 1,
                ));
            }
            hunk.is_end_of_file = true;
            parsed_lines += 1;
            break;
        }
        if matches!(trimmed, "..." | "…") {
            hunk.has_context_lines = true;
            parsed_lines += 1;
            continue;
        }
        match line.as_bytes().first().copied() {
            None => {
                hunk.has_context_lines = true;
                hunk.old_lines.push(String::new());
                hunk.new_lines.push(String::new());
            }
            Some(b' ') => {
                hunk.has_context_lines = true;
                hunk.old_lines.push(line[1..].to_owned());
                hunk.new_lines.push(line[1..].to_owned());
            }
            Some(b'+') => hunk.new_lines.push(line[1..].to_owned()),
            Some(b'-') => hunk.old_lines.push(line[1..].to_owned()),
            _ if !line.starts_with("@@") => {
                hunk.has_context_lines = true;
                hunk.old_lines.push(line.to_owned());
                hunk.new_lines.push(line.to_owned());
            }
            _ if parsed_lines == 0 => {
                return Err(parse_error(
                    format!(
                        "Unexpected line in hunk: '{line}'. Lines must start with ' ' (context), '+' \
						 (add), or '-' (remove)"
                    ),
                    line_number + 1,
                ));
            }
            _ => break,
        }
        parsed_lines += 1;
    }
    if parsed_lines == 0 {
        return Err(parse_error(
            "Hunk does not contain any lines",
            line_number + u32::try_from(start_index).unwrap_or(u32::MAX),
        ));
    }
    strip_line_number_prefixes(&mut hunk);
    Ok(ParseHunkResult {
        hunk,
        lines_consumed: parsed_lines + start_index,
    })
}

fn extract_marker_path(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let parts = rest.split_whitespace().collect::<Vec<_>>();
        let candidate = parts.get(1).or_else(|| parts.first())?;
        return Some(
            candidate
                .strip_prefix("a/")
                .or_else(|| candidate.strip_prefix("b/"))
                .unwrap_or(candidate)
                .to_string(),
        );
    }
    for marker in &MULTI_FILE_MARKERS[..3] {
        if let Some(path) = line.strip_prefix(marker) {
            return Some(path.trim().to_owned());
        }
    }
    None
}

fn count_multi_file_markers(diff: &str) -> usize {
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    let mut paths = BTreeSet::new();
    for line in diff.split('\n') {
        if is_diff_content_line(line) {
            continue;
        }
        let trimmed = line.trim();
        for marker in MULTI_FILE_MARKERS {
            if trimmed.starts_with(marker) {
                if let Some(path) = extract_marker_path(trimmed)
                    && !path.is_empty()
                {
                    paths.insert(path);
                }
                *counts.entry(marker).or_default() += 1;
                break;
            }
        }
    }
    if paths.is_empty() {
        counts.values().copied().max().unwrap_or_default()
    } else {
        paths.len()
    }
}

fn is_unified_diff_metadata_line(line: &str) -> bool {
    DIFF_METADATA_PREFIXES
        .iter()
        .filter(|prefix| !prefix.starts_with("*** "))
        .any(|prefix| line.starts_with(prefix))
}

/// Parse a diff body into hunks.
pub fn parse_diff_hunks(diff: &str) -> Result<Vec<DiffHunk>, EditError> {
    let multi_file_count = count_multi_file_markers(diff);
    if multi_file_count > 1 {
        return Err(EditError::Apply(format!(
            "Diff contains {multi_file_count} file markers. Single-file patches cannot contain \
			 multi-file markers."
        )));
    }
    let normalized_diff = normalize_diff(diff);
    let lines = normalized_diff.split('\n').collect::<Vec<_>>();
    let mut hunks = Vec::new();
    let mut index = 0_usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        let is_diff_content = matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-'));
        if !is_diff_content && is_unified_diff_metadata_line(trimmed) {
            index += 1;
            continue;
        }
        if trimmed.starts_with("@@") && lines[index + 1..].iter().all(|next| next.trim().is_empty())
        {
            break;
        }
        let parsed = parse_one_hunk(
            &lines[index..],
            u32::try_from(index + 1).unwrap_or(u32::MAX),
            true,
        )?;
        hunks.push(parsed.hunk);
        index += parsed.lines_consumed;
    }
    Ok(hunks)
}

//! Unified-diff parsing, prompt scrubbing, and budgeted truncation.

use std::collections::HashSet;

use super::config::{EXCLUDED_FILES, GenerationConfig, LOW_PRIORITY_EXTENSIONS};
use super::text::{cp_len, take_chars, truncate_bytes};

/// Long-line threshold above which machine payloads are collapsed.
pub(crate) const BLOB_LINE_THRESHOLD: usize = 512;

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "go", "py", "js", "ts", "tsx", "jsx", "java", "c", "cpp", "h", "hpp",
];

const DIFF_HEADER_PREFIXES: &[&str] = &[
    "index ",
    "new file",
    "deleted file",
    "rename ",
    "copy ",
    "similarity index",
    "dissimilarity index",
    "old mode",
    "new mode",
    "+++",
    "---",
];

const NONCONTENT_CHANGE_PREFIXES: &[&str] = &[
    "Binary files",
    "rename from",
    "rename to",
    "copy from",
    "copy to",
];

const MANIFEST_FILES: &[&str] = &[
    "cargo.toml",
    "package.json",
    "go.mod",
    "requirements.txt",
    "pyproject.toml",
];

/// Change status derived from git extended headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileStatus {
    Added,
    Deleted,
    Renamed,
    Modified,
}

impl FileStatus {
    /// Lowercase label used in prompt annotations.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Modified => "modified",
        }
    }
}

/// One file section parsed from a unified diff for prompt preparation.
#[derive(Debug, Clone)]
pub(crate) struct ConventionalFileDiff {
    pub filename: String,
    pub header: String,
    pub content: String,
    pub is_binary: bool,
    pub additions: usize,
    pub deletions: usize,
    pub status: FileStatus,
}

impl ConventionalFileDiff {
    fn new(filename: String) -> Self {
        Self {
            filename,
            header: String::new(),
            content: String::new(),
            is_binary: false,
            additions: 0,
            deletions: 0,
            status: FileStatus::Modified,
        }
    }

    /// UTF-8 byte size used by prompt budgets.
    pub(crate) fn size(&self) -> usize {
        self.header.len() + self.content.len()
    }

    /// Deterministic four-character token estimate.
    pub(crate) fn token_estimate(&self) -> usize {
        ((cp_len(&self.header) + cp_len(&self.content)) / 4).max(1)
    }

    /// Context-retention priority; higher values survive truncation first.
    fn priority(&self) -> i64 {
        if self.is_binary {
            return -100;
        }
        let lower = self.filename.to_lowercase();
        if MANIFEST_FILES.iter().any(|name| lower.ends_with(*name)) {
            return 70;
        }
        if lower.contains("prompt") || lower.contains("system") {
            return 100;
        }
        if self.filename.contains("/test")
            || self.filename.contains("test_")
            || self.filename.contains("_test.")
            || self.filename.contains(".test.")
        {
            return 10;
        }
        let extension = if self.filename.contains('.') {
            self.filename.rsplit('.').next().unwrap_or("")
        } else {
            ""
        };
        if LOW_PRIORITY_EXTENSIONS.contains(&extension) {
            return 20;
        }
        if SOURCE_EXTENSIONS.contains(&extension) {
            return 100;
        }
        if matches!(extension, "sql" | "sh" | "bash") {
            return 80;
        }
        50
    }

    /// Truncate content in place while preserving metadata and useful edges.
    pub(crate) fn truncate(&mut self, max_size: usize) {
        if self.size() <= max_size {
            return;
        }
        const SUFFIX: &str = "\n... (truncated)";
        let available = max_size as i64 - self.header.len() as i64 - SUFFIX.len() as i64;
        if available < 50 {
            self.content = "... (truncated)".to_owned();
            return;
        }
        let available = available as usize;
        let lines = split_lines(&self.content);
        if lines.len() > 30 {
            let omitted = lines.len() - 25;
            let mut kept: Vec<String> = Vec::with_capacity(26);
            kept.extend(lines[..15].iter().cloned());
            kept.push(format!("... (truncated {omitted} lines) ..."));
            kept.extend(lines[lines.len() - 10..].iter().cloned());
            let content = kept.join("\n");
            if content.len() <= available {
                self.content = content;
                return;
            }
            self.content = format!("{}{SUFFIX}", truncate_bytes(&content, available));
            return;
        }
        self.content = format!("{}{SUFFIX}", truncate_bytes(&self.content, available));
    }
}

/// Split on `\r\n`, `\n`, or `\r`, dropping one trailing empty element.
pub(crate) fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn format_omitted_size(count: usize) -> String {
    if count >= 1024 * 1024 {
        format!("{:.1}MB", count as f64 / (1024.0 * 1024.0))
    } else if count >= 1024 {
        format!("{:.0}KB", count as f64 / 1024.0)
    } else {
        format!("{count}B")
    }
}

/// Collapse blob-like diff lines while preserving recognizable head and tail text.
pub(crate) fn collapse_blob_lines(diff: &str, threshold: usize) -> String {
    if cp_len(diff) <= threshold {
        return diff.to_owned();
    }
    let mut changed = false;
    let lines: Vec<String> = diff
        .split('\n')
        .map(|line| {
            let length = cp_len(line);
            if length <= threshold {
                return line.to_owned();
            }
            changed = true;
            let omitted = length.saturating_sub(120 + 24);
            let tail: String = line.chars().skip(length.saturating_sub(24)).collect();
            format!(
                "{}[..omitted {}..]{}",
                take_chars(line, 120),
                format_omitted_size(omitted),
                tail
            )
        })
        .collect();
    if changed {
        lines.join("\n")
    } else {
        diff.to_owned()
    }
}

fn stat_change_count(line: &str) -> usize {
    let tail = line.rsplit('|').next().unwrap_or("").trim();
    let digits = tail.split_whitespace().next().unwrap_or("");
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse::<usize>().unwrap_or(0)
    } else {
        0
    }
}

/// Keep the largest stat rows plus the totals line.
pub(crate) fn condense_stat(stat: &str, max_files: usize) -> String {
    let lines: Vec<String> = split_lines(stat)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let file_lines: Vec<String> = lines
        .iter()
        .filter(|line| line.contains('|'))
        .cloned()
        .collect();
    if file_lines.len() <= max_files {
        return stat.to_owned();
    }
    let other_lines: Vec<&String> = lines.iter().filter(|line| !line.contains('|')).collect();
    let mut sorted = file_lines.clone();
    sorted.sort_by_key(|line| std::cmp::Reverse(stat_change_count(line)));
    let kept: HashSet<&str> = sorted.iter().take(max_files).map(String::as_str).collect();
    let mut condensed: Vec<String> = file_lines
        .iter()
        .filter(|line| kept.contains(line.as_str()))
        .cloned()
        .collect();
    let omitted = file_lines.len() - condensed.len();
    if omitted > 0 && !condensed.is_empty() {
        condensed.push(format!(" ... {omitted} more files omitted ..."));
    }
    condensed.extend(other_lines.into_iter().cloned());
    condensed.join("\n")
}

fn flush_section(
    current: &mut Option<ConventionalFileDiff>,
    header_lines: &mut Vec<String>,
    content_lines: &mut Vec<String>,
    files: &mut Vec<ConventionalFileDiff>,
) {
    if let Some(mut file) = current.take() {
        file.header = header_lines.join("\n");
        file.content = content_lines.join("\n");
        files.push(file);
    }
    header_lines.clear();
    content_lines.clear();
}

/// Parse a unified git diff into file-level prompt sections.
pub(crate) fn parse_prompt_diff(diff: &str) -> Vec<ConventionalFileDiff> {
    let mut files = Vec::new();
    let mut current: Option<ConventionalFileDiff> = None;
    let mut header_lines: Vec<String> = Vec::new();
    let mut content_lines: Vec<String> = Vec::new();
    let mut in_header = false;
    for line in split_lines(diff) {
        if line.starts_with("diff --git") {
            flush_section(
                &mut current,
                &mut header_lines,
                &mut content_lines,
                &mut files,
            );
            let filename = diff_header_path(&line).unwrap_or_else(|| "unknown".to_owned());
            current = Some(ConventionalFileDiff::new(filename));
            header_lines.push(line);
            in_header = true;
            continue;
        }
        if current.is_none() {
            continue;
        }
        if line.starts_with("Binary files") {
            if let Some(file) = current.as_mut() {
                file.is_binary = true;
            }
            header_lines.push(line);
        } else if in_header
            && DIFF_HEADER_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(*prefix))
        {
            if let Some(file) = current.as_mut() {
                if line.starts_with("new file") {
                    file.status = FileStatus::Added;
                } else if line.starts_with("deleted file") {
                    file.status = FileStatus::Deleted;
                } else if line.starts_with("rename ") {
                    file.status = FileStatus::Renamed;
                }
            }
            header_lines.push(line);
        } else if line.starts_with("@@") {
            in_header = false;
            content_lines.push(line);
        } else if !in_header {
            if let Some(file) = current.as_mut() {
                if line.starts_with('+') {
                    file.additions += 1;
                } else if line.starts_with('-') {
                    file.deletions += 1;
                }
            }
            content_lines.push(line);
        } else {
            header_lines.push(line);
        }
    }
    flush_section(
        &mut current,
        &mut header_lines,
        &mut content_lines,
        &mut files,
    );
    files
}

/// Reconstruct prompt text from parsed file sections.
pub(crate) fn reconstruct_prompt_diff(files: &[ConventionalFileDiff]) -> String {
    files
        .iter()
        .map(|file| {
            if file.content.is_empty() {
                file.header.clone()
            } else {
                format!("{}\n{}", file.header, file.content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse long payloads and cap any remaining oversized file section.
pub(crate) fn scrub_diff_for_prompt(diff: &str, max_file_bytes: usize) -> String {
    let collapsed = collapse_blob_lines(diff, BLOB_LINE_THRESHOLD);
    if collapsed.len() <= max_file_bytes {
        return collapsed;
    }
    let mut files = parse_prompt_diff(&collapsed);
    if !files.iter().any(|file| file.size() > max_file_bytes) {
        return collapsed;
    }
    for file in &mut files {
        if file.size() > max_file_bytes {
            file.truncate(max_file_bytes);
        }
    }
    reconstruct_prompt_diff(&files)
}

pub(crate) fn is_excluded(filename: &str) -> bool {
    EXCLUDED_FILES
        .iter()
        .any(|pattern| filename.ends_with(*pattern))
}

/// Truncate a diff by file priority while retaining whole-file scope.
pub(crate) fn smart_truncate_diff(
    diff: &str,
    max_length: usize,
    config: &GenerationConfig,
) -> String {
    let mut files: Vec<ConventionalFileDiff> = parse_prompt_diff(diff)
        .into_iter()
        .filter(|file| !is_excluded(&file.filename))
        .collect();
    if files.is_empty() {
        return "No relevant files to analyze (only lock files or excluded files were changed)"
            .to_owned();
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.priority()));
    let total_size: usize = files.iter().map(ConventionalFileDiff::size).sum();
    let total_tokens: usize = files.iter().map(ConventionalFileDiff::token_estimate).sum();
    let effective_max = if total_tokens > config.max_diff_tokens {
        config.max_diff_tokens * 4
    } else {
        max_length
    };
    if total_size <= effective_max {
        return reconstruct_prompt_diff(&files);
    }

    let total_files = files.len();
    let mut included: Vec<ConventionalFileDiff> = Vec::new();
    let header_only_size: usize = files.iter().map(|file| file.header.len() + 20).sum();
    if header_only_size <= effective_max {
        let remaining = effective_max - header_only_size;
        let per_file = remaining / total_files;
        for mut file in files {
            if file.is_binary {
                file.content.clear();
                included.push(file);
                continue;
            }
            let target_size = file.header.len() + per_file;
            if file.size() > target_size {
                file.truncate(target_size);
            }
            included.push(file);
        }
    } else {
        let mut current_size = 0usize;
        for mut file in files {
            if file.is_binary {
                continue;
            }
            if current_size + file.size() <= effective_max {
                current_size += file.size();
                included.push(file);
            } else if current_size * 2 < effective_max && file.priority() >= 50 {
                file.truncate(effective_max.saturating_sub(current_size + 100));
                included.push(file);
                break;
            }
        }
    }

    if included.is_empty() {
        return "Error: Could not include any files in the diff".to_owned();
    }
    let mut result = reconstruct_prompt_diff(&included);
    let excluded_count = total_files - included.len();
    if excluded_count > 0 {
        result.push_str(&format!("\n\n... ({excluded_count} files omitted) ..."));
    }
    result
}

/// Truncate to a line budget distributed by file priority.
pub(crate) fn truncate_diff_by_lines(diff: &str, max_lines: usize) -> String {
    let files = parse_prompt_diff(diff);
    let total_lines: usize = files
        .iter()
        .map(|file| split_lines(&file.header).len() + split_lines(&file.content).len())
        .sum();
    if total_lines <= max_lines {
        return diff.to_owned();
    }
    let total_priority: i64 = files.iter().map(|file| file.priority().max(1)).sum();
    let total_priority = total_priority.max(1);
    let mut result: Vec<String> = Vec::new();
    for file in &files {
        result.extend(split_lines(&file.header));
        let content_lines = split_lines(&file.content);
        let weight = file.priority().max(1);
        let allocated = ((max_lines as i64 * weight) / total_priority).max(5) as usize;
        if content_lines.len() <= allocated {
            result.extend(content_lines.iter().cloned());
            if content_lines.is_empty() {
                result.push(String::new());
            }
            continue;
        }
        let keep_start = allocated / 2;
        let keep_end = allocated - keep_start;
        let omitted = content_lines.len() - keep_start - keep_end;
        result.extend(content_lines[..keep_start].iter().cloned());
        result.push(format!("[... {omitted} lines omitted ...]"));
        result.extend(
            content_lines[content_lines.len() - keep_end..]
                .iter()
                .cloned(),
        );
    }
    let mut joined = result.join("\n");
    if !result.is_empty() {
        joined.push('\n');
    }
    joined
}

/// Diff sections split by `diff --git` at a line start: `(preamble, [(path, section)])`.
fn file_sections(diff: &str) -> (&str, Vec<(String, String)>) {
    const MARKER: &str = "diff --git";
    let starts: Vec<usize> = diff
        .match_indices(MARKER)
        .map(|(index, _)| index)
        .filter(|&index| index == 0 || diff.as_bytes()[index - 1] == b'\n')
        .collect();
    if starts.is_empty() {
        return (diff, Vec::new());
    }
    let sections = starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let end = starts.get(position + 1).copied().unwrap_or(diff.len());
            let section = &diff[start..end];
            let first_line = split_lines(section).into_iter().next().unwrap_or_default();
            let path = diff_header_path(&first_line).unwrap_or_else(|| "unknown".to_owned());
            (path, section.to_owned())
        })
        .collect();
    (&diff[..starts[0]], sections)
}

fn section_is_whitespace_only(section: &str) -> bool {
    let mut added = String::new();
    let mut removed = String::new();
    let mut has_change = false;
    let mut in_hunks = false;
    for line in split_lines(section) {
        if NONCONTENT_CHANGE_PREFIXES
            .iter()
            .any(|prefix| line.starts_with(*prefix))
        {
            return false;
        }
        if line.starts_with("@@") {
            in_hunks = true;
            continue;
        }
        if !in_hunks {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            has_change = true;
            added.extend(rest.chars().filter(|c| !c.is_whitespace()));
        } else if let Some(rest) = line.strip_prefix('-') {
            has_change = true;
            removed.extend(rest.chars().filter(|c| !c.is_whitespace()));
        }
    }
    has_change && added == removed
}

/// Whether every file section of a diff changes only whitespace.
pub(crate) fn is_all_whitespace_diff(diff: &str) -> bool {
    let (_, sections) = file_sections(diff);
    !sections.is_empty()
        && sections
            .iter()
            .all(|(_, section)| section_is_whitespace_only(section))
}

/// Remove whitespace-only file sections, returning `None` when nothing changes.
pub(crate) fn strip_whitespace_only_files(diff: &str) -> Option<String> {
    let (preamble, sections) = file_sections(diff);
    if sections.is_empty() {
        return None;
    }
    let mut kept: Vec<String> = Vec::new();
    let mut stripped_any = false;
    for (_, section) in sections {
        if section_is_whitespace_only(&section) {
            stripped_any = true;
        } else {
            kept.push(section);
        }
    }
    if !stripped_any || kept.is_empty() {
        return None;
    }
    Some(format!("{preamble}{}", kept.concat()))
}

/// One file of a git diff, split for the AI staging classifier.
pub(crate) struct FileDiff {
    pub filename: String,
    pub content: String,
    pub is_binary: bool,
}

/// One `@@` hunk. `index` is 0-based; `content` starts with the `@@` line.
pub(crate) struct DiffHunk {
    pub index: usize,
    pub content: String,
}

/// The new-side path of a `diff --git` header line, decoded. Git C-quotes names with special bytes
/// (`"a/caf\303\251.rs"`), and an unquoted name may itself contain ` b/`. Unquoted sides are split by
/// length, since `--no-renames` diffs print the same name on both sides.
pub(crate) fn diff_header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    let new_side = if rest.starts_with('"') {
        let (_, after_old) = unquote_c(rest)?;
        let new_raw = after_old.strip_prefix(' ')?;
        if new_raw.starts_with('"') {
            unquote_c(new_raw)?.0
        } else {
            new_raw.to_owned()
        }
    } else {
        let body = rest.strip_prefix("a/")?;
        let half = body.len().checked_sub(3)? / 2;
        if body.len() % 2 == 1
            && body.get(half..half + 3) == Some(" b/")
            && body.get(..half) == body.get(half + 3..)
        {
            body[half + 1..].to_owned()
        } else if let Some(index) = body.find(" \"b/") {
            unquote_c(&body[index + 1..])?.0
        } else {
            let (_, new) = body.rsplit_once(" b/")?;
            format!("b/{new}")
        }
    };
    new_side.strip_prefix("b/").map(str::to_owned)
}

/// Decodes the C-quoted token at the start of `text`: the decoded name and the text after the
/// closing quote. Handles git's escapes: `\a \b \t \n \v \f \r \" \\` and three-digit octal bytes.
fn unquote_c(text: &str) -> Option<(String, &str)> {
    let inner = text.strip_prefix('"')?;
    let bytes = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                return Some((
                    String::from_utf8_lossy(&out).into_owned(),
                    &inner[index + 1..],
                ));
            }
            b'\\' => {
                let escape = *bytes.get(index + 1)?;
                let octal = match bytes.get(index + 1..index + 4) {
                    Some(&[a, b, c]) if [a, b, c].iter().all(|d| (b'0'..=b'7').contains(d)) => {
                        let value = u32::from(a - b'0') * 64
                            + u32::from(b - b'0') * 8
                            + u32::from(c - b'0');
                        Some(u8::try_from(value).ok()?)
                    }
                    _ => None,
                };
                if let Some(byte) = octal {
                    out.push(byte);
                    index += 4;
                    continue;
                }
                out.push(match escape {
                    b'a' => 7,
                    b'b' => 8,
                    b't' => 9,
                    b'n' => 10,
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => 13,
                    b'"' => b'"',
                    b'\\' => b'\\',
                    _ => return None,
                });
                index += 2;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    None
}

/// Split a git diff into per-file sections. Each section keeps its trailing newlines verbatim.
pub(crate) fn parse_file_diffs(diff: &str) -> Vec<FileDiff> {
    split_at_file_starts(diff)
        .into_iter()
        .filter_map(|part| {
            if part.trim().is_empty() {
                return None;
            }
            let header = part.split('\n').next().unwrap_or("");
            let filename = diff_header_path(header)?;
            let is_binary = part
                .split('\n')
                .any(|line| line.starts_with("Binary files "));
            Some(FileDiff {
                filename,
                content: part.to_owned(),
                is_binary,
            })
        })
        .collect()
}

/// Parts beginning at each `diff --git ` line start. Text before the first part is dropped,
/// since it never carries a file header.
fn split_at_file_starts(diff: &str) -> Vec<&str> {
    let starts: Vec<usize> = diff
        .match_indices("diff --git ")
        .map(|(index, _)| index)
        .filter(|&index| index == 0 || diff.as_bytes()[index - 1] == b'\n')
        .collect();
    (0..starts.len())
        .map(|position| {
            let end = starts.get(position + 1).copied().unwrap_or(diff.len());
            &diff[starts[position]..end]
        })
        .collect()
}

/// Hunks of one non-binary file. Binary files have none.
pub(crate) fn parse_file_hunks(file: &FileDiff) -> Vec<DiffHunk> {
    if file.is_binary {
        return Vec::new();
    }
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut current: Option<usize> = None;
    let mut buffer: Vec<&str> = Vec::new();
    for line in file.content.split('\n') {
        if line.starts_with("@@") {
            if let Some(index) = current {
                hunks.push(DiffHunk {
                    index,
                    content: buffer.join("\n"),
                });
            }
            buffer.clear();
            buffer.push(line);
            current = Some(hunks.len());
            continue;
        }
        if current.is_some() {
            buffer.push(line);
        }
    }
    if let Some(index) = current {
        hunks.push(DiffHunk {
            index,
            content: buffer.join("\n"),
        });
    }
    hunks
}

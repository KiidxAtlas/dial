//! Bounded workspace search. Pages use sorted component/depth-first file order.
//! `skip` counts results (matching lines, or non-overlapping multiline matches); in the
//! `output="files"`/`"count"` summary modes it counts matching files instead.
//! Copy `next_cursor` and `remaining_skip` from the footer to continue an unchanged
//! query/workspace; cursors are inclusive file positions plus consumed match counts.
//! Coverage is per call; ignored subtrees are explicitly not enumerated.
//! Grep edit evidence uses immutable UTF-8 source snapshots and only complete
//! verbatim rows surviving the final 48 KiB page budget, including its footer.
//! Summary modes show no source rows and so record no edit evidence.
//! Freshness failures retain the observation but grant no authority for that file.

use super::walk::SearchWalk;
use crate::tools::{MAX_LIST, Workspace, io_message, opt_bool, opt_str, opt_u64, req_str};
use anyhow::Result;
use regex::bytes::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const DEFAULT_GREP_MATCHES: usize = 400;
const MAX_GREP_MATCHES: usize = 1000;
const MAX_GREP_CONTEXT: usize = 10;
const MAX_GREP_OUTPUT: usize = 48 * 1024;
const MAX_GREP_FILE: u64 = 4 * 1024 * 1024;
const MAX_GREP_LINE: usize = 300;
const MAX_PATTERN: usize = 512;
const REGEX_SIZE_LIMIT: usize = 1 << 20;
const REGEX_DFA_LIMIT: usize = 8 << 20;
const MAX_GREP_BYTES: u64 = 64 * 1024 * 1024;
/// Matching lines one file may add to a page when `max_results` is defaulted. An explicit
/// `max_results` lifts it; otherwise the rest of that file is named in the footer.
const GREP_PER_FILE: usize = 20;
const GREP_WITNESS_WARNING: &str = "[edit witnesses: some observed files changed or could not be verified; no new edit authority was recorded for those files]\n";
/// Appended to a capped `lines` page: a summary page names every matching file far more cheaply.
const GREP_FILES_HINT: &str = "; output=\"files\" lists matching files with match counts";

/// What a grep page reports: matching rows (default), matching files with counts, or totals.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Lines,
    Files,
    Count,
}

impl OutputMode {
    fn parse(args: &Value) -> Result<Self, String> {
        match opt_str(args, "output")? {
            None | Some("lines") => Ok(Self::Lines),
            Some("files") => Ok(Self::Files),
            Some("count") => Ok(Self::Count),
            Some(other) => Err(format!(
                "output must be \"lines\", \"files\" or \"count\", not {other:?}"
            )),
        }
    }
}

/// Non-overlapping whole-file matches, advancing past empty matches like the multiline page.
fn count_multiline(matcher: &Regex, bytes: &[u8]) -> usize {
    let (mut offset, mut count) = (0, 0);
    while let Some(found) = matcher.find_at(bytes, offset) {
        count += 1;
        if found.end() > found.start() {
            offset = found.end();
        } else if found.end() < bytes.len() {
            offset = found.end() + 1;
        } else {
            break;
        }
    }
    count
}

fn bool_default(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    if args.get(key).is_none_or(Value::is_null) {
        Ok(default)
    } else {
        opt_bool(args, key)
    }
}

/// Literal filenames win over semicolon splitting; validate every scope before searching.
fn scopes(ws: &Workspace, args: &Value) -> Result<Vec<PathBuf>, String> {
    let input = opt_str(args, "path")?.unwrap_or(".");
    let literal = ws.resolve(input)?;
    let inputs = if literal.exists() {
        vec![input]
    } else {
        input.split(';').map(str::trim).collect()
    };
    if inputs.len() > 64 || inputs.iter().any(|s| s.is_empty()) {
        return Err("path accepts at most 64 non-empty semicolon-separated scopes".into());
    }
    let mut paths = Vec::new();
    for input in inputs {
        let path = ws.resolve(input)?;
        // Reject even in-workspace symlink scopes rather than silently changing their identity.
        let original = if Path::new(input).is_absolute() {
            PathBuf::from(input)
        } else {
            ws.root.join(input)
        };
        let mut ancestor = Some(original.as_path());
        while let Some(p) = ancestor {
            if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(format!("search scopes must not follow symlinks: {input:?}"));
            }
            if p == ws.root {
                break;
            }
            ancestor = p.parent();
        }
        if path.strip_prefix(&ws.root).is_ok_and(|relative| {
            relative.components().any(|part| part.as_os_str() == ".git")
                || relative
                    .components()
                    .next()
                    .is_some_and(|part| part.as_os_str() == crate::workspace_copy::SCRATCH_DIR)
        }) {
            return Err("search scopes must not enter .git or workspace scratch".into());
        }
        fs::metadata(&path).map_err(|e| {
            super::suggest::annotate(ws, io_message("searching", input, &e), &[input])
        })?;
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !roots.iter().any(|root| path.starts_with(root)) {
            roots.push(path);
        }
    }
    Ok(roots)
}

fn cursor(args: &Value, ws: &Workspace) -> Result<Option<(PathBuf, usize)>, String> {
    let Some(value) = args.get("cursor").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let path = ws.resolve(req_str(value, "path")?)?;
    let consumed = opt_u64(value, "match")?.unwrap_or(0);
    let consumed = usize::try_from(consumed).map_err(|_| "cursor.match is too large")?;
    Ok(Some((path, consumed)))
}

fn page_footer(
    ws: &Workspace,
    next: Option<&(PathBuf, usize)>,
    remaining_skip: u64,
    coverage: &str,
) -> Result<String, String> {
    let next = next
        .map(|(path, consumed)| {
            let relative = path.strip_prefix(&ws.root).unwrap_or(path);
            let path = relative
                .to_str()
                .ok_or("cannot encode continuation for a non-UTF-8 filename; narrow the scope")?;
            Ok::<_, String>(serde_json::json!({"path": path, "match": consumed}))
        })
        .transpose()?;
    // The traversal order is part of the tool description; repeat it only when there is a
    // continuation to copy, which is when it matters.
    let page = if next.is_none() && remaining_skip == 0 {
        serde_json::json!({"next_cursor": null, "remaining_skip": 0})
    } else {
        serde_json::json!({"next_cursor": next, "remaining_skip": remaining_skip,
            "order": "sorted component/depth-first; unchanged query and workspace required"})
    };
    Ok(format!("[coverage: {coverage}]\n[page: {page}]\n"))
}

fn finish_page(
    mut text: String,
    ws: &Workspace,
    next: Option<(PathBuf, usize)>,
    remaining_skip: u64,
    coverage: &str,
) -> Result<String, String> {
    if text.is_empty() {
        text.push_str("no matches\n");
    }
    text.push_str(&page_footer(ws, next.as_ref(), remaining_skip, coverage)?);
    Ok(text)
}

fn expand_braces(pattern: &str, out: &mut Vec<String>) -> Result<(), String> {
    const MAX_ALTERNATIVES: usize = 64;
    let Some(open) = pattern.find('{') else {
        out.push(pattern.to_string());
        return Ok(());
    };
    let mut depth = 0usize;
    let mut close = None;
    let mut commas = Vec::new();
    for (i, c) in pattern[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            ',' if depth == 1 => commas.push(open + i),
            _ => {}
        }
    }
    let Some(close) = close else {
        out.push(pattern.to_string());
        return Ok(());
    };
    let (prefix, suffix) = (&pattern[..open], &pattern[close + 1..]);
    let mut start = open + 1;
    commas.push(close);
    for end in commas {
        expand_braces(&format!("{prefix}{}{suffix}", &pattern[start..end]), out)?;
        if out.len() > MAX_ALTERNATIVES {
            return Err(format!(
                "glob expands to more than {MAX_ALTERNATIVES} alternatives"
            ));
        }
        start = end + 1;
    }
    Ok(())
}

fn match_segment(pattern: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        let step = match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
                continue;
            }
            Some('?') => Some(1),
            Some('[') => match_class(&pattern[p..], name[n]),
            Some(&c) if c == name[n] => Some(1),
            _ => None,
        };
        match step {
            Some(used) => {
                p += used;
                n += 1;
            }
            None => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// Match `[...]` at the start of `pattern` against `c`; returns the pattern
/// length consumed on a match. A malformed class matches nothing.
fn match_class(pattern: &[char], c: char) -> Option<usize> {
    let mut i = 1;
    let negate = matches!(pattern.get(i), Some('!' | '^'));
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while let Some(&current) = pattern.get(i) {
        if current == ']' && !first {
            return (matched != negate).then_some(i + 1);
        }
        first = false;
        if pattern.get(i + 1) == Some(&'-') && pattern.get(i + 2).is_some_and(|&e| e != ']') {
            matched |= current <= c && c <= pattern[i + 2];
            i += 3;
        } else {
            matched |= current == c;
            i += 1;
        }
    }
    None
}

fn match_segments(pattern: &[Vec<char>], path: &[Vec<char>]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first.iter().eq(['*', '*'].iter()) => {
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((first, rest)) => path
            .split_first()
            .is_some_and(|(name, tail)| match_segment(first, name) && match_segments(rest, tail)),
    }
}

/// Compiled glob: brace alternatives, each a list of segments.
struct Glob {
    alternatives: Vec<Vec<Vec<char>>>,
    /// Common literal directory prefix; an empty prefix leaves the walk unpruned.
    prefix: PathBuf,
}

impl Glob {
    fn new(pattern: &str) -> Result<Glob, String> {
        if pattern.is_empty() || pattern.len() > MAX_PATTERN {
            return Err(format!("glob pattern must be 1..={MAX_PATTERN} bytes"));
        }
        let mut expanded = Vec::new();
        expand_braces(pattern, &mut expanded)?;
        let mut alternatives = Vec::new();
        for mut alt in expanded {
            if alt.starts_with('/') {
                return Err(
                    "glob patterns are relative to the workspace root; remove the leading '/'"
                        .to_string(),
                );
            }
            while let Some(rest) = alt.strip_prefix("./") {
                alt = rest.to_string();
            }
            if alt.split('/').any(|segment| segment == "..") {
                return Err("glob patterns must not contain '..'".to_string());
            }
            if alt.ends_with('/') {
                alt.push_str("**");
            }
            if !alt.contains('/') {
                alt = format!("**/{alt}");
            }
            let mut segments: Vec<Vec<char>> = Vec::new();
            for segment in alt.split('/').filter(|s| !s.is_empty()) {
                let chars: Vec<char> = segment.chars().collect();
                // Adjacent `**` segments are equivalent to one; collapsing keeps matching linear-ish.
                if chars == ['*', '*'] && segments.last().is_some_and(|l| l == &chars) {
                    continue;
                }
                segments.push(chars);
            }
            alternatives.push(segments);
        }
        let mut common = alternatives[0][..alternatives[0].len().saturating_sub(1)]
            .iter()
            .take_while(|segment| !segment.iter().any(|c| matches!(c, '*' | '?' | '[')))
            .cloned()
            .collect::<Vec<_>>();
        for alternative in &alternatives[1..] {
            let shared = common
                .iter()
                .zip(alternative)
                .take_while(|(a, b)| a == b)
                .count();
            common.truncate(shared.min(alternative.len().saturating_sub(1)));
        }
        let prefix = common
            .iter()
            .map(|segment| segment.iter().collect::<String>())
            .collect();
        Ok(Glob {
            alternatives,
            prefix,
        })
    }

    fn matches(&self, relative: &str) -> bool {
        let path: Vec<Vec<char>> = relative.split('/').map(|s| s.chars().collect()).collect();
        self.alternatives
            .iter()
            .any(|alt| match_segments(alt, &path))
    }
}

pub(in crate::tools) fn glob_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
    let glob = Glob::new(req_str(args, "pattern")?)?;
    let roots = scopes(ws, args)?;
    let resume = cursor(args, ws)?;
    if resume.as_ref().is_some_and(|(_, consumed)| *consumed != 0) {
        return Err("glob cursor.match must be 0".into());
    }
    let limit = opt_u64(args, "limit")?.unwrap_or(MAX_LIST as u64);
    if limit == 0 || limit > MAX_LIST as u64 {
        return Err(format!("limit must be 1..={MAX_LIST}"));
    }
    let mut skip = opt_u64(args, "skip")?.unwrap_or(0);
    let mut walk = SearchWalk::new(
        ws,
        bool_default(args, "hidden", true)?,
        bool_default(args, "gitignore", true)?,
        resume.map(|(p, _)| p),
    );
    let mut text = String::new();
    let mut count = 0;
    let mut next = None;
    let mut unrepresentable = 0;
    for root in roots {
        walk.run_with_prefix(&root, &ws.root.join(&glob.prefix), &mut |path| {
            if path.to_str().is_none() {
                unrepresentable += 1;
                return true;
            }
            if !glob.matches(&ws.rel(path)) {
                return true;
            }
            if skip > 0 {
                skip -= 1;
                return true;
            }
            let row = format!("{}\n", ws.rel(path));
            if count == limit || text.len() + row.len() > MAX_GREP_OUTPUT {
                next = Some((path.to_path_buf(), 0));
                return false;
            }
            text.push_str(&row);
            count += 1;
            true
        });
        if next.is_some() || walk.next.is_some() {
            break;
        }
    }
    next = next.or_else(|| walk.next.clone().map(|p| (p, 0)));
    finish_page(
        text,
        ws,
        next,
        skip,
        &if unrepresentable == 0 {
            format!("results={count}; {}", walk.coverage())
        } else {
            format!(
                "results={count}; non_utf8_filenames_excluded={unrepresentable}; {}",
                walk.coverage()
            )
        },
    )
}

/// Compile once for bounded byte matching, on lines or whole files.
/// `fixed_string` escapes all metacharacters. Anchors work at line boundaries;
/// regex compile memory and per-call input work are bounded separately.
fn build_matcher(pattern: &str, fixed_string: bool, ignore_case: bool) -> Result<Regex, String> {
    let source = if fixed_string {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    RegexBuilder::new(&source)
        .case_insensitive(ignore_case)
        .multi_line(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_LIMIT)
        .build()
        .map_err(|e| format!("invalid or unsupported Rust regex: {e}\nLookaround and backreferences are unsupported; simplify the pattern or use `fixed_string: true`."))
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

struct FormattedRow {
    text: String,
    complete: bool,
}

/// One output row: `path:N:text` for a match, `path-N-text` for context.
/// The text is bounded to [`MAX_GREP_LINE`] characters.
fn format_row(rel: &str, number: usize, sep: char, lead: &str, line: &[u8]) -> FormattedRow {
    let head = &line[..line.len().min(MAX_GREP_LINE * 4)];
    let text = String::from_utf8_lossy(head);
    let (shown, cut) = match text.char_indices().nth(MAX_GREP_LINE) {
        Some((at, _)) => (&text[..at], true),
        None => (&text[..], head.len() < line.len()),
    };
    FormattedRow {
        text: format!(
            "{rel}{sep}{number}{sep}{lead}{shown}{}\n",
            if cut { " …[truncated]" } else { "" }
        ),
        complete: lead.is_empty() && !cut && std::str::from_utf8(line).is_ok(),
    }
}

fn format_source_row(rel: &str, number: usize, sep: char, raw: &[u8]) -> FormattedRow {
    let mut row = format_row(rel, number, sep, "", trim_eol(raw));
    // Rendering removes bare terminal CR too; edit normalization removes only CRLF.
    row.complete &= !raw.ends_with(b"\r");
    row
}

struct CompleteRow {
    number: usize,
    end: usize,
}

struct GrepFile {
    path: PathBuf,
    source: Option<String>,
    rows: Vec<CompleteRow>,
    header: Option<(usize, usize)>,
}

struct PageMatch {
    file: usize,
    consumed: usize,
    start: usize,
    end: usize,
}

/// What became of one match: shown on the page, held back (skipped or past its file's cap), or
/// the page must stop and resume at it.
enum Emitted {
    Shown,
    Hidden,
    Stop,
}

struct GrepState {
    matcher: Regex,
    multiline: bool,
    context: usize,
    max_matches: usize,
    out: String,
    matches: usize,
    skip: u64,
    resume: Option<(PathBuf, usize)>,
    next: Option<(PathBuf, usize)>,
    scanned: usize,
    unreadable: usize,
    oversized: usize,
    binary: usize,
    bytes: u64,
    context_capped: usize,
    line_starts: Vec<usize>,
    files: Vec<GrepFile>,
    file_matches: HashMap<PathBuf, usize>,
    matched_files: HashSet<PathBuf>,
    page_matches: Vec<PageMatch>,
    row_ends: Vec<usize>,
    blocked_row: bool,
    /// Why this page stopped before the scan ended; named in the footer when a continuation exists.
    cap: Option<String>,
    /// The most matching lines one file may add to this page; unbounded when max_results is explicit.
    per_file: usize,
    /// Files whose matches were held back by `per_file`, in walk order (one entry per file).
    capped_files: Vec<PathBuf>,
    mode: OutputMode,
    /// Summary modes: matching files counted on this page, and the listed `(path, matches)` rows.
    summary_files: usize,
    summary_rows: Vec<(PathBuf, usize)>,
}

impl GrepState {
    fn match_row(&self, rel: &str, number: usize, raw: &[u8]) -> FormattedRow {
        let line = trim_eol(raw);
        match self.matcher.find(line) {
            Some(found) if found.start() >= MAX_GREP_LINE => {
                let mut start = found.start() - MAX_GREP_LINE / 3;
                while start < line.len() && (line[start] & 0xC0) == 0x80 {
                    start += 1;
                }
                format_row(
                    rel,
                    number,
                    ':',
                    &format!("…[{start} bytes skipped] "),
                    &line[start..],
                )
            }
            _ => format_source_row(rel, number, ':', raw),
        }
    }

    /// A match and its preceding context are committed together. Never advance
    /// the result cursor until the matching row itself is visible. A match that is
    /// held back (skipped, or past its file's cap) is `Hidden`: the walk goes on.
    fn emit(&mut self, path: &Path, consumed: usize, matched: &str, before: &str) -> Emitted {
        if self.skip > 0 {
            self.skip -= 1;
            return Emitted::Hidden;
        }
        let file_count = self.file_matches.get(path).copied().unwrap_or(0);
        if file_count >= self.per_file {
            if self.capped_files.last().map(PathBuf::as_path) != Some(path) {
                self.capped_files.push(path.to_path_buf());
            }
            return Emitted::Hidden;
        }
        if self.matches >= self.max_matches
            || self.out.len() + matched.len() > MAX_GREP_OUTPUT
            || (!self.out.is_empty()
                && self.out.len() + before.len() + matched.len() > MAX_GREP_OUTPUT)
        {
            self.blocked_row = self.out.is_empty() && matched.len() > MAX_GREP_OUTPUT;
            self.cap = Some(if self.matches >= self.max_matches {
                format!("match limit of {}", self.max_matches)
            } else {
                "output size limit of 48 KiB".to_string()
            });
            self.next = Some((path.to_path_buf(), consumed));
            return Emitted::Stop;
        }
        *self.file_matches.entry(path.to_path_buf()).or_default() += 1;
        self.matched_files.insert(path.to_path_buf());
        let start = self.out.len();
        if self.out.len() + before.len() + matched.len() <= MAX_GREP_OUTPUT {
            self.out.push_str(before);
        } else {
            self.context_capped += 1;
        }
        self.out.push_str(matched);
        self.matches += 1;
        self.page_matches.push(PageMatch {
            file: self.files.len(),
            consumed,
            start,
            end: self.out.len(),
        });
        self.row_ends.push(self.out.len());
        Emitted::Shown
    }

    fn search_file(&mut self, ws: &Workspace, path: &Path, bytes: &mut Vec<u8>) {
        if self
            .resume
            .as_ref()
            .is_some_and(|(p, _)| path < p.as_path())
        {
            return;
        }
        let consumed = self
            .resume
            .as_ref()
            .filter(|(p, _)| p == path)
            .map_or(0, |(_, n)| *n);
        // Repeat confinement validation immediately before opening, and reject symlinks.
        if path.to_str().is_none()
            || ws.resolve(&ws.rel(path)).is_err()
            || fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        {
            self.unreadable += 1;
            return;
        }
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                self.unreadable += 1;
                return;
            }
        };
        if !meta.is_file() {
            return;
        }
        let partial = meta.len() > MAX_GREP_FILE;
        if self.bytes + MAX_GREP_FILE > MAX_GREP_BYTES {
            self.next = Some((path.to_path_buf(), consumed));
            return;
        }
        let mut file = match super::walk::open_search_file(path) {
            Ok(file) => file,
            Err(_) => {
                self.unreadable += 1;
                return;
            }
        };
        bytes.clear();
        let prefix_result = file.by_ref().take(8192).read_to_end(bytes);
        self.bytes += bytes.len() as u64;
        if prefix_result.is_err() {
            self.unreadable += 1;
            return;
        }
        if bytes.contains(&0) {
            self.binary += 1;
            return;
        }
        let before_read = bytes.len();
        let result = file
            .take(MAX_GREP_FILE - bytes.len() as u64)
            .read_to_end(bytes);
        self.bytes += (bytes.len() - before_read) as u64;
        if result.is_err() {
            self.unreadable += 1;
            return;
        }
        if partial {
            self.oversized += 1;
        }
        self.scanned += 1;
        if self.mode != OutputMode::Lines {
            self.summarize(ws, path, bytes);
            return;
        }
        let rel = ws.rel(path);
        let start = self.out.len();
        let match_start = self.page_matches.len();
        let row_start = self.row_ends.len();
        let mut rows = Vec::new();
        if self.multiline {
            self.search_multiline(path, &rel, bytes, consumed, &mut rows);
        } else {
            self.search_lines(path, &rel, bytes, consumed, &mut rows);
        }
        if self.out.len() > start {
            let mut header_span = None;
            // Move the exact read snapshot, never reconstruct it from rendered rows.
            let source = if rows.is_empty() {
                None
            } else {
                String::from_utf8(std::mem::take(bytes)).ok()
            };
            if let Some(source) = &source {
                let normalized = crate::tools::edit::hashline::normalize(source);
                let header = format!(
                    "[{}#{}]\n",
                    rel,
                    crate::tools::edit::hashline::tag_of(&normalized)
                );
                self.out.insert_str(start, &header);
                header_span = Some((start, start + header.len()));
                for matched in &mut self.page_matches[match_start..] {
                    matched.start += header.len();
                    matched.end += header.len();
                }
                for end in &mut self.row_ends[row_start..] {
                    *end += header.len();
                }
                for row in &mut rows {
                    row.end += header.len();
                }
            }
            self.files.push(GrepFile {
                path: path.to_path_buf(),
                source,
                rows,
                header: header_span,
            });
        }
    }

    /// Count one file's matches for a summary page. `skip` and the page limits count files.
    fn summarize(&mut self, ws: &Workspace, path: &Path, bytes: &[u8]) {
        let count = if self.multiline {
            count_multiline(&self.matcher, bytes)
        } else {
            bytes
                .split_inclusive(|&b| b == b'\n')
                .filter(|raw| self.matcher.is_match(trim_eol(raw)))
                .count()
        };
        if count == 0 {
            return;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        if self.mode == OutputMode::Files {
            let row = format!(
                "{} ({count} {})\n",
                ws.rel(path),
                if count == 1 { "match" } else { "matches" }
            );
            let full = self.summary_rows.len() >= self.max_matches;
            if full || self.out.len() + row.len() > MAX_GREP_OUTPUT {
                self.cap = Some(if full {
                    format!("file limit of {}", self.max_matches)
                } else {
                    "output size limit of 48 KiB".to_string()
                });
                self.next = Some((path.to_path_buf(), 0));
                return;
            }
            self.out.push_str(&row);
            self.row_ends.push(self.out.len());
            self.summary_rows.push((path.to_path_buf(), count));
        }
        self.summary_files += 1;
        self.matches += count;
    }

    /// Footer for summary modes: the same coverage, cap and continuation semantics as `lines`,
    /// with files and matches counted instead of rows shown.
    fn finish_summary(
        mut self,
        ws: &Workspace,
        mut next: Option<(PathBuf, usize)>,
        walk_coverage: &str,
        walk_complete: bool,
    ) -> Result<String, String> {
        let too_large =
            || "grep summary and continuation exceed 48 KiB; narrow the scope".to_string();
        loop {
            let limited =
                !walk_complete || next.is_some() || self.unreadable > 0 || self.oversized > 0;
            let omitted = if next.is_some() {
                let cap = match &self.cap {
                    Some(cap) => cap.as_str(),
                    None if self.bytes + MAX_GREP_FILE > MAX_GREP_BYTES => "byte limit of 64 MiB",
                    None => "walk limit",
                };
                format!(
                    "; page_capped_by={cap}; files and matches past the cap are omitted from this page and not counted; continue by repeating this grep with cursor=next_cursor and skip=remaining_skip from [page]"
                )
            } else {
                String::new()
            };
            let coverage = if limited {
                format!(
                    "files={}; matches={}; scanned_files={}; bytes={}; unreadable_files={}; partial_files={}; binary_files={}; file_limit=4MiB; byte_limit=64MiB; {walk_coverage}{omitted}; omitted coverage is not exhaustive",
                    self.summary_files,
                    self.matches,
                    self.scanned,
                    self.bytes,
                    self.unreadable,
                    self.oversized,
                    self.binary,
                )
            } else {
                format!(
                    "files={}; matches={}; scanned_files={}; binary_files={}; {walk_coverage}",
                    self.summary_files, self.matches, self.scanned, self.binary,
                )
            };
            let footer = page_footer(ws, next.as_ref(), self.skip, &coverage)?;
            let body = if self.summary_files == 0 {
                "no matches\n".to_string()
            } else if self.mode == OutputMode::Count {
                format!(
                    "{} {} in {} {}\n",
                    self.matches,
                    if self.matches == 1 {
                        "match"
                    } else {
                        "matches"
                    },
                    self.summary_files,
                    if self.summary_files == 1 {
                        "file"
                    } else {
                        "files"
                    },
                )
            } else {
                std::mem::take(&mut self.out)
            };
            if body.len() + footer.len() <= MAX_GREP_OUTPUT {
                return Ok(body + &footer);
            }
            // Only a files page can shrink: drop its last row and resume at that file.
            self.out = body;
            let Some((path, count)) = self
                .summary_rows
                .pop()
                .filter(|_| self.mode == OutputMode::Files)
            else {
                return Err(too_large());
            };
            self.row_ends.pop();
            self.out
                .truncate(self.row_ends.last().copied().unwrap_or(0));
            self.summary_files -= 1;
            self.matches -= count;
            self.cap = Some("output size limit of 48 KiB".to_string());
            next = Some((path, 0));
        }
    }
    fn search_lines(
        &mut self,
        path: &Path,
        rel: &str,
        bytes: &[u8],
        consumed: usize,
        rows: &mut Vec<CompleteRow>,
    ) {
        let mut before: VecDeque<(usize, &[u8])> = VecDeque::with_capacity(self.context);
        let mut last_printed = None;
        let mut after_left = 0;
        let mut seen = 0;
        let mut preceding = String::new();
        let mut preceding_rows = Vec::with_capacity(self.context);
        for (index, raw) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
            let (number, line) = (index + 1, trim_eol(raw));
            if self.matcher.is_match(line) {
                seen += 1;
                if seen <= consumed {
                    if self.context > 0 {
                        if before.len() == self.context {
                            before.pop_front();
                        }
                        before.push_back((number, raw));
                    }
                    continue;
                }
                let first = before.front().map_or(number, |&(n, _)| n);
                let gap = last_printed.map_or(!self.out.is_empty(), |last| first > last + 1);
                preceding.clear();
                preceding_rows.clear();
                if self.context > 0 && gap {
                    preceding.push_str("--\n");
                }
                for &(n, context_line) in &before {
                    let row = format_source_row(rel, n, '-', context_line);
                    preceding.push_str(&row.text);
                    if row.complete {
                        preceding_rows.push((n, preceding.len()));
                    }
                }
                let row = self.match_row(rel, number, raw);
                let start = self.out.len();
                let shown = match self.emit(path, seen - 1, &row.text, &preceding) {
                    Emitted::Stop => return,
                    Emitted::Shown => true,
                    Emitted::Hidden => false,
                };
                if shown {
                    if self.out.len() - start == preceding.len() + row.text.len() {
                        rows.extend(preceding_rows.iter().map(|&(number, end)| CompleteRow {
                            number,
                            end: start + end,
                        }));
                    }
                    if row.complete {
                        rows.push(CompleteRow {
                            number,
                            end: self.out.len(),
                        });
                    }
                    before.clear();
                    last_printed = Some(number);
                    after_left = self.context;
                } else if self.context > 0 {
                    if before.len() == self.context {
                        before.pop_front();
                    }
                    before.push_back((number, raw));
                }
            } else if after_left > 0 {
                let row = format_source_row(rel, number, '-', raw);
                if self.out.len() + row.text.len() <= MAX_GREP_OUTPUT {
                    self.out.push_str(&row.text);
                    self.row_ends.push(self.out.len());
                    if row.complete {
                        rows.push(CompleteRow {
                            number,
                            end: self.out.len(),
                        });
                    }
                    last_printed = Some(number);
                } else {
                    self.context_capped += 1;
                }
                after_left -= 1;
            } else if self.context > 0 {
                if before.len() == self.context {
                    before.pop_front();
                }
                before.push_back((number, raw));
            }
        }
    }

    fn search_multiline(
        &mut self,
        path: &Path,
        rel: &str,
        bytes: &[u8],
        consumed: usize,
        rows: &mut Vec<CompleteRow>,
    ) {
        self.line_starts.clear();
        self.line_starts.push(0);
        self.line_starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        let mut offset = 0;
        let mut seen = 0;
        let mut last_end: Option<usize> = None;
        let mut preceding = String::new();
        let mut preceding_rows = Vec::with_capacity(self.context);
        while let Some(found) = self.matcher.find_at(bytes, offset) {
            let (start, end) = (found.start(), found.end());
            seen += 1;
            let first = self
                .line_starts
                .partition_point(|&at| at <= start)
                .saturating_sub(1);
            let last = self
                .line_starts
                .partition_point(|&at| at <= end.saturating_sub(1).max(start))
                .saturating_sub(1);
            if seen > consumed {
                preceding.clear();
                preceding_rows.clear();
                let from = first.saturating_sub(self.context);
                if self.context > 0 && last_end.map_or(!self.out.is_empty(), |n| from > n + 1) {
                    preceding.push_str("--\n");
                }
                for index in from..first {
                    if last_end.is_some_and(|n| index <= n) {
                        continue;
                    }
                    let to = self
                        .line_starts
                        .get(index + 1)
                        .copied()
                        .unwrap_or(bytes.len());
                    let row =
                        format_source_row(rel, index + 1, '-', &bytes[self.line_starts[index]..to]);
                    preceding.push_str(&row.text);
                    if row.complete && self.line_starts[index] < bytes.len() {
                        preceding_rows.push((index + 1, preceding.len()));
                    }
                }
                // This escaped match-only summary is never complete source evidence,
                // even when its reported inclusive line span covers whole lines.
                let excerpt = &bytes[start..end.min(start.saturating_add(MAX_GREP_LINE * 4))];
                let escaped = String::from_utf8_lossy(excerpt)
                    .replace('\r', "\\r")
                    .replace('\n', "\\n");
                let row = format_row(
                    rel,
                    first + 1,
                    ':',
                    &format!(
                        "{}:{} ",
                        last + 1,
                        if end - start > excerpt.len() {
                            " [match truncated]"
                        } else {
                            ""
                        }
                    ),
                    escaped.as_bytes(),
                );
                let output_start = self.out.len();
                let shown = match self.emit(path, seen - 1, &row.text, &preceding) {
                    Emitted::Stop => return,
                    Emitted::Shown => true,
                    Emitted::Hidden => false,
                };
                if shown {
                    if self.out.len() - output_start == preceding.len() + row.text.len() {
                        rows.extend(preceding_rows.iter().map(|&(number, end)| CompleteRow {
                            number,
                            end: output_start + end,
                        }));
                    }
                    last_end = Some(last);
                    for index in last + 1..=(last + self.context).min(self.line_starts.len() - 1) {
                        let to = self
                            .line_starts
                            .get(index + 1)
                            .copied()
                            .unwrap_or(bytes.len());
                        let row = format_source_row(
                            rel,
                            index + 1,
                            '-',
                            &bytes[self.line_starts[index]..to],
                        );
                        if self.out.len() + row.text.len() <= MAX_GREP_OUTPUT {
                            self.out.push_str(&row.text);
                            self.row_ends.push(self.out.len());
                            if row.complete && self.line_starts[index] < bytes.len() {
                                rows.push(CompleteRow {
                                    number: index + 1,
                                    end: self.out.len(),
                                });
                            }
                        } else {
                            self.context_capped += 1;
                        }
                        last_end = Some(index);
                    }
                }
            }
            if end > start {
                offset = end;
            } else if end < bytes.len() {
                offset = end + 1;
            } else {
                break;
            }
        }
    }

    fn finish(
        mut self,
        ws: &Workspace,
        mut next: Option<(PathBuf, usize)>,
        walk_coverage: &str,
        walk_complete: bool,
    ) -> Result<String, String> {
        let too_large = || {
            "grep result and continuation exceed 48 KiB; narrow the scope or context".to_string()
        };
        if self.blocked_row {
            return Err(too_large());
        }
        let footer = loop {
            let limited = !walk_complete
                || next.is_some()
                || !self.capped_files.is_empty()
                || self.unreadable > 0
                || self.oversized > 0
                || self.context_capped > 0;
            // Name what the page left out and how to resume. Counts past the cap are not known
            // without scanning on, so the footer says so instead of guessing.
            let mut omitted = if next.is_some() {
                format!(
                    "; page_capped_by={}; page_files_with_matches={}; matches and files past the cap are omitted from this page and not counted; continue by repeating this grep with cursor=next_cursor and skip=remaining_skip from [page]",
                    self.cap.as_deref().unwrap_or("output size limit of 48 KiB"),
                    self.matched_files.len(),
                )
            } else {
                String::new()
            };
            if !self.capped_files.is_empty() {
                let listed: Vec<String> = self
                    .capped_files
                    .iter()
                    .take(8)
                    .map(|path| {
                        path.strip_prefix(&ws.root)
                            .unwrap_or(path)
                            .display()
                            .to_string()
                    })
                    .collect();
                let more = self.capped_files.len().saturating_sub(listed.len());
                let more = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };
                omitted.push_str(&format!(
                    "; per_file_cap={} reached in: {}{more}; rows past it are not shown; repeat with path=<file> and an explicit max_results (which lifts the per-file cap) to see them",
                    self.per_file,
                    listed.join(", "),
                ));
            }
            if next.is_some() || !self.capped_files.is_empty() {
                omitted.push_str(GREP_FILES_HINT);
            }
            let walk_coverage = format!("{walk_coverage}{omitted}");
            let coverage = if limited {
                format!(
                    "results={}; scanned_files={}; bytes={}; unreadable_files={}; partial_files={}; binary_files={}; context_rows_capped={}; file_limit=4MiB; byte_limit=64MiB; {}; omitted coverage is not exhaustive",
                    self.matches,
                    self.scanned,
                    self.bytes,
                    self.unreadable,
                    self.oversized,
                    self.binary,
                    self.context_capped,
                    walk_coverage,
                )
            } else {
                format!(
                    "results={}; scanned_files={}; binary_files={}; {walk_coverage}",
                    self.matches, self.scanned, self.binary,
                )
            };
            let footer = page_footer(ws, next.as_ref(), self.skip, &coverage)?;
            let budget = MAX_GREP_OUTPUT
                .checked_sub(footer.len() + GREP_WITNESS_WARNING.len())
                .ok_or_else(too_large)?;
            let body_len = if self.out.is_empty() {
                "no matches\n".len()
            } else {
                self.out.len()
            };
            if body_len <= budget {
                break footer;
            }
            while self
                .page_matches
                .last()
                .is_some_and(|matched| matched.end > budget)
            {
                let matched = self.page_matches.pop().unwrap();
                next = Some((self.files[matched.file].path.clone(), matched.consumed));
                let cut = match self.files[matched.file].header {
                    Some((at, end)) if end == matched.start => at,
                    _ => matched.start,
                };
                self.out.truncate(cut);
                self.matches -= 1;
                let kept = self.row_ends.partition_point(|&end| end <= matched.start);
                self.context_capped += (self.row_ends.len() - kept).saturating_sub(1);
                self.row_ends.truncate(kept);
            }
            if self.page_matches.is_empty() {
                return Err(too_large());
            }
            if self.out.len() > budget {
                let kept = self.row_ends.partition_point(|&end| end <= budget);
                self.context_capped += self.row_ends.len() - kept;
                let cut = match kept.checked_sub(1) {
                    Some(index) => self.row_ends[index],
                    None => self.page_matches.last().map_or(0, |matched| matched.end),
                };
                self.out.truncate(cut);
                self.row_ends.truncate(kept);
            }
        };
        let returned_body = self.out.len();
        if self.out.is_empty() {
            self.out.push_str("no matches\n");
        }
        // Footer validation and all row removals precede any witness publication.
        // The fixed warning reserve cannot evict a row after it has been recorded.
        let mut witness_failed = false;
        for file in self.files {
            let Some(source) = file.source else { continue };
            let mut numbers: Vec<_> = file
                .rows
                .into_iter()
                .filter(|row| row.end <= returned_body)
                .map(|row| row.number)
                .collect();
            if numbers.is_empty() {
                continue;
            }
            numbers.sort_unstable();
            numbers.dedup();
            let mut ranges: Vec<(usize, usize)> = Vec::new();
            for number in numbers {
                if let Some(last) = ranges.last_mut().filter(|last| number == last.1 + 1) {
                    last.1 = number;
                } else {
                    ranges.push((number, number));
                }
            }
            if ws
                .record_edit_witnesses(&file.path, &source, &ranges)
                .is_err()
            {
                witness_failed = true;
            }
        }
        if witness_failed {
            self.out.push_str(GREP_WITNESS_WARNING);
        }
        self.out.push_str(&footer);
        Ok(self.out)
    }
}

pub(in crate::tools) fn grep_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
    let pattern = req_str(args, "pattern")?;
    if pattern.is_empty() || pattern.len() > MAX_PATTERN {
        return Err(format!("pattern must be 1..={MAX_PATTERN} bytes"));
    }
    let fixed = opt_bool(args, "fixed_string")?;
    let escaped_newline = !fixed
        && pattern.as_bytes().windows(2).enumerate().any(|(i, pair)| {
            pair == b"\\n"
                && pattern.as_bytes()[..i]
                    .iter()
                    .rev()
                    .take_while(|&&b| b == b'\\')
                    .count()
                    % 2
                    == 0
        });
    let multiline = opt_bool(args, "multiline")? || pattern.contains('\n') || escaped_newline;
    let matcher = build_matcher(pattern, fixed, opt_bool(args, "ignore_case")?)?;
    if let Some(path) = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| crate::tools::integrations::ssh::url::is_ssh_url(path))
    {
        if multiline {
            return Err("multiline grep is unavailable for ssh:// paths".into());
        }
        let user_dir = crate::tools::integrations::ssh::user_dir()?;
        let text = crate::tools::integrations::ssh::read_url(
            path,
            &user_dir,
            ws.ssh_config(),
            ws.ssh_hosts(),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("ssh:// scheme checked above")?;
        let skip = opt_u64(args, "skip")?.unwrap_or(0) as usize;
        let max = opt_u64(args, "max_results")?
            .unwrap_or(DEFAULT_GREP_MATCHES as u64)
            .min(MAX_GREP_MATCHES as u64) as usize;
        if max == 0 {
            return Err(format!("max_results must be 1..={MAX_GREP_MATCHES}"));
        }
        if opt_u64(args, "context")?.unwrap_or(0) != 0 {
            return Err("context lines are not supported for ssh:// grep".into());
        }
        let mut out = String::new();
        let mut seen = 0usize;
        let mut matched = 0usize;
        let mut capped = false;
        for (index, line) in text.lines().enumerate() {
            if !matcher.is_match(line.as_bytes()) {
                continue;
            }
            if seen < skip {
                seen += 1;
                continue;
            }
            if matched == max {
                capped = true;
                break;
            }
            matched += 1;
            out.push_str(&format!("{path}:{}:{}\n", index + 1, line));
            if out.len() >= MAX_GREP_OUTPUT {
                let mut end = MAX_GREP_OUTPUT;
                while !out.is_char_boundary(end) {
                    end -= 1;
                }
                out.truncate(end);
                out.push_str("\n[output capped]\n");
                capped = true;
                break;
            }
        }
        if matched == 0 {
            out.push_str("No matches.\n");
        } else if capped {
            out.push_str("[results capped; increase skip to continue]\n");
        }
        return Ok(out);
    }
    let mode = OutputMode::parse(args)?;
    let roots = scopes(ws, args)?;
    let resume = cursor(args, ws)?;
    if mode != OutputMode::Lines && resume.as_ref().is_some_and(|(_, consumed)| *consumed != 0) {
        return Err("cursor.match must be 0 for output=\"files\" or \"count\"".into());
    }
    let context = opt_u64(args, "context")?
        .unwrap_or(0)
        .min(MAX_GREP_CONTEXT as u64) as usize;
    let max_matches = opt_u64(args, "max_results")?
        .unwrap_or(DEFAULT_GREP_MATCHES as u64)
        .min(MAX_GREP_MATCHES as u64);
    if max_matches == 0 {
        return Err(format!("max_results must be 1..={MAX_GREP_MATCHES}"));
    }
    let glob = opt_str(args, "glob")?.map(Glob::new).transpose()?;
    let mut walk = SearchWalk::new(
        ws,
        bool_default(args, "hidden", true)?,
        bool_default(args, "gitignore", true)?,
        resume.as_ref().map(|(p, _)| p.clone()),
    );
    let per_file = if args
        .get("max_results")
        .is_some_and(|value| !value.is_null())
    {
        usize::MAX
    } else {
        GREP_PER_FILE
    };
    let mut state = GrepState {
        matcher,
        multiline,
        context,
        max_matches: max_matches as usize,
        out: String::new(),
        matches: 0,
        skip: opt_u64(args, "skip")?.unwrap_or(0),
        resume,
        next: None,
        scanned: 0,
        unreadable: 0,
        oversized: 0,
        binary: 0,
        bytes: 0,
        context_capped: 0,
        line_starts: Vec::new(),
        files: Vec::new(),
        file_matches: HashMap::new(),
        matched_files: HashSet::new(),
        page_matches: Vec::new(),
        row_ends: Vec::new(),
        blocked_row: false,
        cap: None,
        per_file,
        capped_files: Vec::new(),
        mode,
        summary_files: 0,
        summary_rows: Vec::new(),
    };
    let mut bytes = Vec::new();
    for root in roots {
        walk.run(&root, &mut |path| {
            if glob.as_ref().is_none_or(|g| g.matches(&ws.rel(path))) {
                state.search_file(ws, path, &mut bytes);
            }
            state.next.is_none()
        });
        if state.next.is_some() || walk.next.is_some() {
            break;
        }
    }
    let next = state
        .next
        .take()
        .or_else(|| walk.next.clone().map(|p| (p, 0)));
    if mode == OutputMode::Lines {
        state.finish(ws, next, &walk.coverage(), walk.is_complete())
    } else {
        state.finish_summary(ws, next, &walk.coverage(), walk.is_complete())
    }
}

#[cfg(test)]
mod tests;

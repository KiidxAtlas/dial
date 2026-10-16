//! Bounded workspace search. Pages use sorted component/depth-first file order.
//! `skip` counts results (matching lines, or non-overlapping multiline matches).
//! Copy `next_cursor` and `remaining_skip` from the footer to continue an unchanged
//! query/workspace; cursors are inclusive file positions plus consumed match counts.
//! Coverage is per call; ignored subtrees are explicitly not enumerated.
//! Grep edit evidence uses immutable UTF-8 source snapshots and only complete
//! verbatim rows surviving the final 48 KiB page budget, including its footer.
//! Freshness failures retain the observation but grant no authority for that file.

use super::walk::SearchWalk;
use super::{MAX_LIST, Workspace, io_message, opt_bool, opt_str, opt_u64, req_str};
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
                    .is_some_and(|part| part.as_os_str() == super::SCRATCH_DIR)
        }) {
            return Err("search scopes must not enter .git or workspace scratch".into());
        }
        fs::metadata(&path).map_err(|e| io_message("searching", input, &e))?;
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

pub(super) fn glob_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
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
                let normalized = super::hashline::normalize(source);
                let header = format!("[{}#{}]\n", rel, super::hashline::tag_of(&normalized));
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

pub(super) fn grep_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
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
        .filter(|path| super::ssh::url::is_ssh_url(path))
    {
        if multiline {
            return Err("multiline grep is unavailable for ssh:// paths".into());
        }
        let user_dir = super::ssh::user_dir()?;
        let text = super::ssh::read_url(
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
    let roots = scopes(ws, args)?;
    let resume = cursor(args, ws)?;
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
    state.finish(ws, next, &walk.coverage(), walk.is_complete())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    fn body(text: &str) -> String {
        let body = text.split("[coverage:").next().unwrap_or(text);
        let filtered: Vec<_> = body
            .lines()
            .filter(|line| {
                let Some((path, tag)) = line
                    .strip_prefix('[')
                    .and_then(|line| line.strip_suffix(']'))
                    .and_then(|line| line.rsplit_once('#'))
                else {
                    return true;
                };
                path.is_empty()
                    || tag.len() != 4
                    || !tag.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .collect();
        let mut result = filtered.join("\n");
        if body.ends_with('\n') {
            result.push('\n');
        }
        result
    }

    #[test]
    fn nested_linked_worktrees_are_skipped_but_submodules_are_not() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("main.rs"), "needle\n").unwrap();
        let copy = dir.path().join(".claude/worktrees/feature");
        fs::create_dir_all(&copy).unwrap();
        fs::write(copy.join(".git"), "gitdir: /repo/.git/worktrees/feature\n").unwrap();
        fs::write(copy.join("main.rs"), "needle\n").unwrap();
        let module = dir.path().join("vendor/lib");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join(".git"), "gitdir: ../../.git/modules/lib\n").unwrap();
        fs::write(module.join("lib.rs"), "needle\n").unwrap();

        let glob = run(&ws, "glob", json!({"pattern": "**/*.rs"})).text;
        assert_eq!(body(&glob), "main.rs\nvendor/lib/lib.rs\n", "{glob}");
        let grep = grep(&ws, json!({"pattern": "needle"}));
        assert!(!grep.contains(".claude/worktrees"), "{grep}");
        assert!(grep.contains("vendor/lib/lib.rs"), "{grep}");
    }

    #[test]
    fn complete_walks_report_a_short_footer_and_limited_walks_keep_details() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let text = grep(&ws, json!({"pattern": "needle"}));
        let footer = text.split("[coverage:").nth(1).unwrap();
        assert!(footer.contains("walk complete"), "{text}");
        assert!(!footer.contains("directory_entry_limit"), "{text}");
        assert!(footer.contains(r#""next_cursor":null"#), "{text}");
        assert!(!footer.contains("order"), "{text}");
        for i in 0..3 {
            fs::write(dir.path().join(format!("m{i}.txt")), "needle\n").unwrap();
        }
        let capped = grep(&ws, json!({"pattern": "needle", "max_results": 1}));
        let footer = capped.split("[coverage:").nth(1).unwrap();
        assert!(
            footer.contains("omitted coverage is not exhaustive"),
            "{capped}"
        );
        assert!(footer.contains(r#""order""#), "{capped}");
    }
    #[cfg(unix)]
    #[test]
    fn glob_and_grep_do_not_follow_symlinks() {
        let (dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("leak.txt"), "needle outside\n").unwrap();
        symlink(outside.path(), dir.path().join("linkdir")).unwrap();
        symlink(
            outside.path().join("leak.txt"),
            dir.path().join("linkfile.txt"),
        )
        .unwrap();
        fs::write(dir.path().join("real.txt"), "needle inside\n").unwrap();

        let glob = run(&ws, "glob", json!({"pattern": "**/*.txt"}));
        assert_eq!(body(&glob.text).trim(), "real.txt");
        let grep = run(&ws, "grep", json!({"pattern": "needle"}));
        assert!(grep.text.contains("real.txt:1:needle inside"));
        assert!(!grep.text.contains("outside"));
        let direct = run(&ws, "grep", json!({"pattern": "needle", "path": "linkdir"}));
        assert!(direct.is_error);
    }

    #[test]
    fn glob_patterns_sort_and_cap() {
        let (dir, ws) = workspace();
        for path in [
            "src/b.rs",
            "src/a.rs",
            "src/deep/c.rs",
            "README.md",
            "docs/x.md",
            ".git/config.rs",
            ".tmp/t.rs",
        ] {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, "").unwrap();
        }
        let text = |p: &str| {
            let result = run(&ws, "glob", json!({"pattern": p})).text;
            let rows = body(&result);
            if rows == "no matches\n" {
                "no matches".to_string()
            } else {
                rows.to_string()
            }
        };
        assert_eq!(text("src/*.rs"), "src/a.rs\nsrc/b.rs\n");
        assert_eq!(text("src/**/*.rs"), "src/a.rs\nsrc/b.rs\nsrc/deep/c.rs\n");
        assert_eq!(text("*.rs"), "src/a.rs\nsrc/b.rs\nsrc/deep/c.rs\n");
        assert_eq!(
            text("*.{md,rs}"),
            "README.md\ndocs/x.md\nsrc/a.rs\nsrc/b.rs\nsrc/deep/c.rs\n"
        );
        assert_eq!(text("src/[ab].rs"), "src/a.rs\nsrc/b.rs\n");
        assert_eq!(text("src/?.rs"), "src/a.rs\nsrc/b.rs\n");
        assert_eq!(text("docs/"), "docs/x.md\n");
        assert_eq!(text("nothing*"), "no matches");
        assert!(run(&ws, "glob", json!({"pattern": "../*"})).is_error);
        assert!(run(&ws, "glob", json!({"pattern": "/etc/*"})).is_error);
        for i in 0..(MAX_LIST + 20) {
            fs::write(dir.path().join(format!("many_{i:04}.dat")), "").unwrap();
        }
        let many = run(&ws, "glob", json!({"pattern": "many_*.dat"})).text;
        assert_eq!(body(&many).lines().count(), MAX_LIST);
        let footer = many
            .lines()
            .find_map(|line| {
                line.strip_prefix("[page: ")
                    .and_then(|s| s.strip_suffix(']'))
            })
            .unwrap();
        let page: Value = serde_json::from_str(footer).unwrap();
        assert_eq!(
            page["next_cursor"],
            json!({"path": "many_0500.dat", "match": 0})
        );
    }

    fn grep(ws: &Workspace, args: serde_json::Value) -> String {
        run(ws, "grep", args).text
    }

    fn numbered_file(dir: &tempfile::TempDir, name: &str, lines: usize) {
        let body: String = (1..=lines).map(|i| format!("l{i}\n")).collect();
        fs::write(dir.path().join(name), body).unwrap();
    }

    #[test]
    fn grep_fixed_string_treats_metacharacters_literally() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.txt"), "Hello World\nfn a(x) {}\nax\n").unwrap();
        let literal = grep(&ws, json!({"pattern": "a(x)", "fixed_string": true}));
        assert!(
            literal.contains("a.txt:2:fn a(x) {}") && !literal.contains("a.txt:3:"),
            "{literal}"
        );
        assert!(
            grep(&ws, json!({"pattern": ".*", "fixed_string": true})).starts_with("no matches\n")
        );
        let folded = grep(
            &ws,
            json!({"pattern": "HELLO", "fixed_string": true, "ignore_case": true}),
        );
        assert!(body(&folded).starts_with("a.txt:1:Hello World"), "{folded}");
    }

    #[test]
    fn grep_is_a_case_sensitive_regex_by_default() {
        let (dir, ws) = workspace();
        fs::write(
            dir.path().join("a.rs"),
            "fn alpha(x: u8) {}\nlet fn_ptr = 1;\nHELLO\nhello\n",
        )
        .unwrap();
        let calls = grep(&ws, json!({"pattern": r"^fn \w+\("}));
        assert!(
            calls.contains("a.rs:1:fn alpha(x: u8)") && !calls.contains("a.rs:2:"),
            "{calls}"
        );
        let alternation = grep(&ws, json!({"pattern": "alpha|fn_ptr"}));
        assert!(
            alternation.contains("a.rs:1:") && alternation.contains("a.rs:2:"),
            "{alternation}"
        );
        // `a(x)` is a group here: it matches `ax`, not the literal text.
        assert!(grep(&ws, json!({"pattern": "a(l)pha"})).contains("a.rs:1:"));
        assert!(grep(&ws, json!({"pattern": "a(x)"})).starts_with("no matches\n"));
        let sensitive = grep(&ws, json!({"pattern": "hello"}));
        assert!(
            sensitive.contains("a.rs:4:hello") && !sensitive.contains("a.rs:3:"),
            "{sensitive}"
        );
        let folded = grep(&ws, json!({"pattern": "^hello$", "ignore_case": true}));
        assert!(
            folded.contains("a.rs:3:HELLO") && folded.contains("a.rs:4:hello"),
            "{folded}"
        );
    }

    #[test]
    fn grep_reports_invalid_regex_without_panicking() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.txt"), "x(\n").unwrap();
        let bad = run(&ws, "grep", json!({"pattern": "x("}));
        assert!(bad.is_error, "{}", bad.text);
        // The same text is fine as a literal.
        assert!(grep(&ws, json!({"pattern": "x(", "fixed_string": true})).contains("a.txt:1:x("));
        let huge = run(&ws, "grep", json!({"pattern": "((a{100}){100}){100}"}));
        assert!(huge.is_error, "{}", huge.text);
        assert!(run(&ws, "grep", json!({"pattern": ""})).is_error);
        assert!(run(&ws, "grep", json!({"pattern": "x", "max_results": 0})).is_error);
    }

    #[test]
    fn grep_filters_by_glob_and_skips_binary_files() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join("a.txt"), "Hello World\n").unwrap();
        fs::write(dir.path().join("b.md"), "hello again\n").unwrap();
        fs::write(dir.path().join("bin.dat"), b"hello\0").unwrap();
        let all = grep(&ws, json!({"pattern": "hello"}));
        assert!(
            all.contains("[b.md#")
                && all.contains("b.md:1:hello again")
                && !all.contains("a.txt")
                && all.contains("binary_files=1"),
            "{all}"
        );
        let only_txt = grep(&ws, json!({"pattern": "(?i)hello", "glob": "*.txt"}));
        assert!(
            only_txt.contains("[a.txt#")
                && only_txt.contains("a.txt:1:Hello World")
                && !only_txt.contains("b.md"),
            "{only_txt}"
        );
        let single = grep(&ws, json!({"pattern": "again", "path": "b.md"}));
        assert!(single.contains("b.md:1:hello again"));
    }

    #[test]
    fn grep_context_lines_merge_and_separate_groups() {
        let (dir, ws) = workspace();
        numbered_file(&dir, "f.txt", 10);
        let one = grep(&ws, json!({"pattern": "l5$", "context": 1}));
        assert_eq!(body(&one), "f.txt-4-l4\nf.txt:5:l5\nf.txt-6-l6\n");
        // Distant matches are split by `--`; overlapping windows merge.
        let apart = grep(&ws, json!({"pattern": "l[27]$", "context": 1}));
        assert_eq!(
            body(&apart),
            "f.txt-1-l1\nf.txt:2:l2\nf.txt-3-l3\n--\nf.txt-6-l6\nf.txt:7:l7\nf.txt-8-l8\n"
        );
        let merged = grep(&ws, json!({"pattern": "l[24]$", "context": 1}));
        assert_eq!(
            body(&merged),
            "f.txt-1-l1\nf.txt:2:l2\nf.txt-3-l3\nf.txt:4:l4\nf.txt-5-l5\n"
        );
        // Windows are clipped at both ends of the file.
        assert_eq!(
            body(&grep(&ws, json!({"pattern": "l1$", "context": 2}))),
            "f.txt:1:l1\nf.txt-2-l2\nf.txt-3-l3\n"
        );
        assert_eq!(
            body(&grep(&ws, json!({"pattern": "l10$", "context": 2}))),
            "f.txt-8-l8\nf.txt-9-l9\nf.txt:10:l10\n"
        );
        // No context means plain match rows with no separators.
        assert_eq!(
            body(&grep(&ws, json!({"pattern": "l[27]$"}))),
            "f.txt:2:l2\nf.txt:7:l7\n"
        );
    }

    #[test]
    fn grep_context_separates_files_and_counts_only_matches() {
        let (dir, ws) = workspace();
        numbered_file(&dir, "a.txt", 3);
        numbered_file(&dir, "b.txt", 3);
        let two = grep(&ws, json!({"pattern": "l2$", "context": 1}));
        assert_eq!(
            body(&two),
            "a.txt-1-l1\na.txt:2:l2\na.txt-3-l3\n--\nb.txt-1-l1\nb.txt:2:l2\nb.txt-3-l3\n"
        );
        // max_results limits matches, not the context rows around them.
        let one = grep(
            &ws,
            json!({"pattern": "l2$", "context": 1, "max_results": 1}),
        );
        assert!(
            body(&one).starts_with("a.txt-1-l1\na.txt:2:l2\na.txt-3-l3\n"),
            "{one}"
        );
        assert!(
            !body(&one).contains("b.txt")
                && one.contains("results=1")
                && one.contains("\"next_cursor\":{"),
            "{one}"
        );
    }

    #[test]
    fn grep_caps_matches_and_bounds_each_line() {
        let (dir, ws) = workspace();
        let many: String = (0..1000).map(|_| "hit\n").collect();
        fs::write(dir.path().join("many.log"), many).unwrap();
        // Without max_results, one file contributes at most GREP_PER_FILE matches to the page; the
        // rest of it is named in the footer, and the walk goes on to later files.
        let capped = grep(&ws, json!({"pattern": "hit", "path": "many.log"}));
        assert_eq!(capped.matches("many.log:").count(), GREP_PER_FILE);
        assert!(
            capped.contains(&format!(
                "per_file_cap={GREP_PER_FILE} reached in: many.log"
            )),
            "{capped}"
        );
        assert!(capped.contains("\"next_cursor\":null"), "{capped}");
        let five = grep(
            &ws,
            json!({"pattern": "hit", "path": "many.log", "max_results": 5}),
        );
        assert_eq!(five.matches("many.log:").count(), 5);
        // An explicit max_results lifts the per-file cap; an oversized one is clamped, not refused.
        let raised = grep(
            &ws,
            json!({"pattern": "hit", "path": "many.log", "max_results": 5000}),
        );
        assert_eq!(
            raised.matches("many.log:").count(),
            MAX_GREP_MATCHES,
            "an oversized max_results is clamped, not refused"
        );
        fs::write(
            dir.path().join("long.txt"),
            format!("needle{}\n", "z".repeat(5000)),
        )
        .unwrap();
        let long = grep(&ws, json!({"pattern": "needle", "path": "long.txt"}));
        assert!(
            body(&long).len() < 400 && long.contains("…[truncated]"),
            "{}",
            long.len()
        );
    }

    #[test]
    fn walks_honor_gitignore_and_skip_dependency_dirs() {
        let (dir, ws) = workspace();
        fs::write(dir.path().join(".gitignore"), "/vendored/\n*.log\n").unwrap();
        for path in [
            "src/a.rs",
            "vendored/lib.rs",
            "src/debug.log",
            "web/node_modules/x.js",
            ".venv/lib/y.py",
            "src/.hidden.rs",
        ] {
            let full = dir.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, "needle\n").unwrap();
        }
        let listed = glob_tool(&ws, &json!({"pattern": "**/*"})).unwrap();
        let names: Vec<&str> = listed.lines().collect();
        assert!(
            names.contains(&"src/a.rs") && names.contains(&"src/.hidden.rs"),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("vendored")
                || n.ends_with(".log")
                || n.contains("node_modules")
                || n.contains(".venv")),
            "{names:?}"
        );
        let found = grep_tool(&ws, &json!({"pattern": "needle"})).unwrap();
        assert!(
            found.contains("src/a.rs")
                && !found.contains("vendored")
                && !found.contains("node_modules")
                && !found.contains(".venv"),
            "{found}"
        );
        let explicit = grep_tool(&ws, &json!({"pattern": "needle", "path": "vendored"})).unwrap();
        assert!(
            explicit.contains("vendored/lib.rs"),
            "an explicit path inside an ignored directory is still searched: {explicit}"
        );
    }

    #[test]
    fn a_match_far_into_a_long_line_is_shown_not_cut_off() {
        let (dir, ws) = workspace();
        let line = format!("{}NEEDLE_HERE{}\n", "x".repeat(1000), "y".repeat(1000));
        fs::write(dir.path().join("min.js"), line).unwrap();
        let out = grep_tool(&ws, &json!({"pattern": "NEEDLE_HERE"})).unwrap();
        assert!(
            out.contains("NEEDLE_HERE") && out.contains("bytes skipped"),
            "{out}"
        );
        assert!(body(&out).len() < 600, "{}", body(&out).len());
        // A match near the start keeps the plain prefix.
        fs::write(
            dir.path().join("short.js"),
            format!("NEEDLE_HERE{}\n", "z".repeat(1000)),
        )
        .unwrap();
        let near = grep_tool(&ws, &json!({"pattern": "NEEDLE_HERE", "path": "short.js"})).unwrap();
        assert!(
            near.contains("short.js:1:NEEDLE_HERE") && !body(&near).contains("skipped"),
            "{near}"
        );
    }

    #[test]
    fn grep_footer_names_the_files_a_capped_page_left_out() {
        let (dir, ws) = workspace();
        // A file past its per-file cap does not end the page: later files still appear on it.
        fs::write(dir.path().join("a_many.txt"), "hit\n".repeat(30)).unwrap();
        for i in 0..25 {
            fs::write(dir.path().join(format!("f{i:02}.txt")), "hit\n").unwrap();
        }
        let capped = grep(&ws, json!({"pattern": "hit"}));
        assert!(capped.contains("f24.txt:1:hit"), "{capped}");
        assert!(
            capped.contains(&format!(
                "per_file_cap={GREP_PER_FILE} reached in: a_many.txt"
            )),
            "{capped}"
        );
        assert!(!capped.contains("page_capped_by"), "{capped}");
        let whole = grep(
            &ws,
            json!({"pattern": "hit", "path": "a_many.txt", "max_results": 5}),
        );
        assert!(whole.contains("page_capped_by=match limit of 5"), "{whole}");
        let small = grep(&ws, json!({"pattern": "NOPE_NEVER"}));
        assert!(!small.contains("page_capped_by"), "{small}");
    }

    #[test]
    fn capped_glob_keeps_deterministic_depth_first_paths() {
        let (dir, ws) = workspace();
        fs::create_dir(dir.path().join("a")).unwrap();
        let mut names = vec!["a.rs".to_string()];
        fs::write(dir.path().join("a.rs"), "").unwrap();
        for index in 0..MAX_LIST + 3 {
            let name = format!("a/{index:04}.rs");
            fs::write(dir.path().join(&name), "").unwrap();
            names.push(name);
        }
        names.sort_by(|a, b| Path::new(a).cmp(Path::new(b)));
        let expected = format!("{}\n", names[..MAX_LIST].join("\n"));
        for _ in 0..3 {
            assert_eq!(
                body(&glob_tool(&ws, &json!({"pattern": "**/*.rs"})).unwrap()),
                expected
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn anchored_globs_preserve_ignore_and_symlink_boundaries() {
        let (dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("mod.rs"), "needle\n").unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        fs::write(dir.path().join(".gitignore"), "/ignored/\n").unwrap();
        for path in [
            "src/deep/mod.rs",
            "docs/mod.rs",
            "ignored/mod.rs",
            "target/mod.rs",
        ] {
            let file = dir.path().join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, "needle\n").unwrap();
        }
        for prefix in ["ignored", "target", "link"] {
            assert_eq!(
                body(&glob_tool(&ws, &json!({"pattern": format!("{prefix}/**/*.rs")})).unwrap()),
                "no matches\n"
            );
        }
        assert_eq!(
            body(&glob_tool(&ws, &json!({"pattern": "src/**/mod.rs"})).unwrap()),
            "src/deep/mod.rs\n"
        );
        assert_eq!(
            body(&glob_tool(&ws, &json!({"pattern": "{src,docs}/**/mod.rs"})).unwrap()),
            "docs/mod.rs\nsrc/deep/mod.rs\n"
        );
        assert_eq!(
            body(&grep(
                &ws,
                json!({"pattern": "needle", "glob": "src/**/mod.rs"})
            )),
            "src/deep/mod.rs:1:needle\n"
        );
    }

    #[test]
    fn grep_binary_probe_boundary_and_reused_buffer() {
        let (dir, ws) = workspace();
        let mut binary = vec![b'x'; 1024 * 1024];
        binary[8191] = 0;
        fs::write(dir.path().join("a.bin"), binary).unwrap();
        let mut text = vec![b'x'; 8192];
        text.extend_from_slice(b"\0\nneedle\n");
        fs::write(dir.path().join("b.txt"), text).unwrap();
        fs::write(dir.path().join("c.txt"), "needle\n").unwrap();
        let result = grep(&ws, json!({"pattern": "needle"}));
        assert_eq!(
            body(&result).replace(GREP_WITNESS_WARNING, ""),
            "b.txt:2:needle\nc.txt:1:needle\n"
        );
        // b.txt carries a NUL past the 8 KiB probe, so it is scanned as text but cannot be
        // witnessed by the edit transaction (which rejects binary); the warning must say so.
        assert!(result.contains(GREP_WITNESS_WARNING), "{result}");
        assert!(result.contains("binary_files=1"));
    }

    #[test]
    fn grep_cap_preserves_deterministic_depth_first_order() {
        let (dir, ws) = workspace();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/z.rs"), "hit first\nhit second\n").unwrap();
        fs::write(dir.path().join("a.rs"), "hit later\n").unwrap();
        let expected = "a/z.rs:1:hit first\n";
        for _ in 0..3 {
            assert_eq!(
                body(&grep(&ws, json!({"pattern": "hit", "max_results": 1}))),
                expected
            );
        }
    }
}

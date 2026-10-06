//! Workspace search tools: `glob` and `grep`.
//!
//! `grep` matches a Rust `regex` (or, with `fixed_string`, literal text) line
//! by line, optionally with surrounding context lines.

use super::walk::Walk;
use super::{MAX_LIST, Workspace, io_message, opt_bool, opt_str, opt_u64, req_str};
use anyhow::Result;
use regex::bytes::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::{BinaryHeap, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const DEFAULT_GREP_MATCHES: usize = 200;
const MAX_GREP_MATCHES: usize = 1000;
const MAX_GREP_CONTEXT: usize = 10;
const MAX_GREP_OUTPUT: usize = 20 * 1024;
const MAX_GREP_FILE: u64 = 2 * 1024 * 1024;
const MAX_GREP_LINE: usize = 300;
const MAX_PATTERN: usize = 512;
const REGEX_SIZE_LIMIT: usize = 1 << 20;
const REGEX_DFA_LIMIT: usize = 8 << 20;

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
    // Sorted depth-first walking is deterministic, but is not full-path lexical order:
    // `a/z.rs` is visited before `a.rs`. Keep the smallest MAX_LIST paths in a max-heap;
    // still count all matches so the existing exact omitted-count diagnostic stays correct.
    let mut matches = BinaryHeap::<String>::with_capacity(MAX_LIST);
    let mut total = 0;
    let mut walk = Walk::new(ws);
    walk.run_with_prefix(&ws.root, &ws.root.join(&glob.prefix), &mut |path| {
        let rel = path
            .strip_prefix(&ws.root)
            .unwrap_or(path)
            .to_string_lossy();
        if glob.matches(&rel) {
            total += 1;
            if matches.len() < MAX_LIST {
                matches.push(rel.into_owned());
            } else if matches
                .peek()
                .is_some_and(|last| rel.as_ref() < last.as_str())
            {
                *matches.peek_mut().unwrap() = rel.into_owned();
            }
        }
        true
    });
    let matches = matches.into_sorted_vec();
    if matches.is_empty() {
        return Ok(if walk.truncated {
            "no matches (workspace walk was truncated at its entry limit)".to_string()
        } else {
            "no matches".to_string()
        });
    }
    let mut text = matches
        .iter()
        .take(MAX_LIST)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    if total > MAX_LIST {
        text.push_str(&format!(
            "[{} more matches not shown; narrow the pattern]\n",
            total - MAX_LIST
        ));
    }
    if walk.truncated {
        text.push_str("[workspace walk truncated at its entry limit; results may be incomplete]\n");
    }
    Ok(text)
}

/// Compile `pattern` for per-line byte matching. `fixed_string` escapes it so
/// every character is literal. Size limits bound compile-time memory; matching
/// itself is linear in the input, so hostile patterns cannot hang the search.
fn build_matcher(pattern: &str, fixed_string: bool, ignore_case: bool) -> Result<Regex, String> {
    let source = if fixed_string {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    RegexBuilder::new(&source)
        .case_insensitive(ignore_case)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_DFA_LIMIT)
        .build()
        .map_err(|e| format!("invalid regex: {e}\nSearch literal text with `fixed_string: true`."))
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// One output row: `path:N:text` for a match, `path-N-text` for context.
/// The text is bounded to [`MAX_GREP_LINE`] characters.
fn format_row(rel: &str, number: usize, sep: char, lead: &str, line: &[u8]) -> String {
    let head = &line[..line.len().min(MAX_GREP_LINE * 4)];
    let text = String::from_utf8_lossy(head);
    let (shown, cut) = match text.char_indices().nth(MAX_GREP_LINE) {
        Some((at, _)) => (&text[..at], true),
        None => (&text[..], head.len() < line.len()),
    };
    format!(
        "{rel}{sep}{number}{sep}{lead}{shown}{}\n",
        if cut { " …[truncated]" } else { "" }
    )
}

struct GrepState {
    matcher: Regex,
    context: usize,
    max_matches: usize,
    out: String,
    matches: usize,
    skipped: usize,
    full: bool,
}

impl GrepState {
    /// The row for a matching line. When the match starts past the width limit, show the text around
    /// it (and how many bytes were skipped) instead of a prefix that never contains the match.
    fn match_row(&self, rel: &str, number: usize, line: &[u8]) -> String {
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
            _ => format_row(rel, number, ':', "", line),
        }
    }

    /// Append `text` unless it would pass the output cap; false once full.
    fn push(&mut self, text: &str) -> bool {
        if self.out.len() + text.len() > MAX_GREP_OUTPUT {
            self.full = true;
            return false;
        }
        self.out.push_str(text);
        true
    }

    /// Emit the `--` separator before a group that does not touch the previous one.
    fn separate(&mut self, last_printed: Option<usize>, first: usize) -> bool {
        let gap = match last_printed {
            Some(last) => first > last + 1,
            None => !self.out.is_empty(),
        };
        !(self.context > 0 && gap) || self.push("--\n")
    }

    fn search_file(&mut self, ws: &Workspace, path: &Path, bytes: &mut Vec<u8>) {
        let Ok(meta) = fs::metadata(path) else { return };
        if meta.len() > MAX_GREP_FILE {
            self.skipped += 1;
            return;
        }
        let Ok(mut file) = fs::File::open(path) else {
            return;
        };
        bytes.clear();
        // Inspect the binary prefix before reading the rest; retain this buffer across files.
        if file.by_ref().take(8192).read_to_end(bytes).is_err() {
            return;
        }
        if bytes.contains(&0) {
            self.skipped += 1;
            return;
        }
        // A file may grow after metadata; bound the read as well as the initial size check.
        if file
            .take(MAX_GREP_FILE + 1 - bytes.len() as u64)
            .read_to_end(bytes)
            .is_err()
        {
            return;
        }
        if bytes.len() as u64 > MAX_GREP_FILE {
            self.skipped += 1;
            return;
        }
        let rel = ws.rel(path);
        let mut before: VecDeque<(usize, &[u8])> = VecDeque::new();
        let mut last_printed: Option<usize> = None;
        let mut after_left = 0;
        for (index, raw) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
            let (number, line) = (index + 1, trim_eol(raw));
            if self.matcher.is_match(line) {
                if self.matches >= self.max_matches {
                    self.full = true;
                    return;
                }
                let first = before.front().map_or(number, |&(n, _)| n);
                if !self.separate(last_printed, first) {
                    return;
                }
                for (n, context_line) in before.drain(..) {
                    if !self.push(&format_row(&rel, n, '-', "", context_line)) {
                        return;
                    }
                }
                if !self.push(&self.match_row(&rel, number, line)) {
                    return;
                }
                self.matches += 1;
                (last_printed, after_left) = (Some(number), self.context);
            } else if after_left > 0 {
                if !self.push(&format_row(&rel, number, '-', "", line)) {
                    return;
                }
                (last_printed, after_left) = (Some(number), after_left - 1);
            } else if self.context > 0 {
                if before.len() == self.context {
                    before.pop_front();
                }
                before.push_back((number, line));
            }
        }
    }
}

pub(super) fn grep_tool(ws: &Workspace, args: &Value) -> Result<String, String> {
    let pattern = req_str(args, "pattern")?;
    if pattern.is_empty() || pattern.len() > MAX_PATTERN {
        return Err(format!("pattern must be 1..={MAX_PATTERN} bytes"));
    }
    let matcher = build_matcher(
        pattern,
        opt_bool(args, "fixed_string")?,
        opt_bool(args, "ignore_case")?,
    )?;
    let context = opt_u64(args, "context")?
        .unwrap_or(0)
        .min(MAX_GREP_CONTEXT as u64) as usize;
    let max_matches = match opt_u64(args, "max_results")? {
        None => DEFAULT_GREP_MATCHES,
        Some(0) => return Err("max_results must be at least 1".to_string()),
        Some(n) => n.min(MAX_GREP_MATCHES as u64) as usize,
    };
    let glob = opt_str(args, "glob")?.map(Glob::new).transpose()?;
    let start = match opt_str(args, "path")? {
        Some(input) => ws.resolve(input)?,
        None => ws.root.clone(),
    };
    let meta = fs::metadata(&start).map_err(|e| io_message("searching", &ws.rel(&start), &e))?;
    let mut state = GrepState {
        matcher,
        context,
        max_matches,
        out: String::new(),
        matches: 0,
        skipped: 0,
        full: false,
    };
    let mut truncated = false;
    let mut bytes = Vec::new();
    if meta.is_file() {
        state.search_file(ws, &start, &mut bytes);
    } else {
        let mut walk = Walk::new(ws);
        let prefix = glob
            .as_ref()
            .map_or_else(|| ws.root.clone(), |g| ws.root.join(&g.prefix));
        walk.run_with_prefix(&start, &prefix, &mut |path| {
            if glob.as_ref().is_none_or(|g| {
                let rel = path
                    .strip_prefix(&ws.root)
                    .unwrap_or(path)
                    .to_string_lossy();
                g.matches(&rel)
            }) {
                state.search_file(ws, path, &mut bytes);
            }
            !state.full
        });
        truncated = walk.truncated;
    }
    let mut text = state.out;
    if text.is_empty() {
        text.push_str("no matches\n");
    }
    if state.full {
        text.push_str(&format!(
            "[stopped at {} matches (max_results {max_matches}) or 20 KiB; narrow with path, glob or a more specific pattern]\n",
            state.matches
        ));
    }
    if state.skipped > 0 {
        text.push_str(&format!(
            "[{} binary or oversized files skipped]\n",
            state.skipped
        ));
    }
    if truncated {
        text.push_str("[workspace walk truncated at its entry limit; results may be incomplete]\n");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::*;
    use serde_json::json;
    use std::os::unix::fs::symlink;
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
        assert_eq!(glob.text.trim(), "real.txt");
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
        let text = |p: &str| run(&ws, "glob", json!({"pattern": p})).text;
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
        let many = text("many_*.dat");
        assert_eq!(many.lines().count(), MAX_LIST + 1);
        assert!(many.contains("20 more matches"));
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
        assert!(folded.starts_with("a.txt:1:Hello World"), "{folded}");
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
        assert!(
            bad.is_error && bad.text.contains("invalid regex") && bad.text.contains("fixed_string"),
            "{}",
            bad.text
        );
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
            all.contains("b.md:1:hello again")
                && !all.contains("a.txt")
                && all.contains("1 binary"),
            "{all}"
        );
        let only_txt = grep(&ws, json!({"pattern": "(?i)hello", "glob": "*.txt"}));
        assert!(
            only_txt.starts_with("a.txt:1:Hello World") && !only_txt.contains("b.md"),
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
        assert_eq!(one, "f.txt-4-l4\nf.txt:5:l5\nf.txt-6-l6\n");
        // Distant matches are split by `--`; overlapping windows merge.
        let apart = grep(&ws, json!({"pattern": "l[27]$", "context": 1}));
        assert_eq!(
            apart,
            "f.txt-1-l1\nf.txt:2:l2\nf.txt-3-l3\n--\nf.txt-6-l6\nf.txt:7:l7\nf.txt-8-l8\n"
        );
        let merged = grep(&ws, json!({"pattern": "l[24]$", "context": 1}));
        assert_eq!(
            merged,
            "f.txt-1-l1\nf.txt:2:l2\nf.txt-3-l3\nf.txt:4:l4\nf.txt-5-l5\n"
        );
        // Windows are clipped at both ends of the file.
        assert_eq!(
            grep(&ws, json!({"pattern": "l1$", "context": 2})),
            "f.txt:1:l1\nf.txt-2-l2\nf.txt-3-l3\n"
        );
        assert_eq!(
            grep(&ws, json!({"pattern": "l10$", "context": 2})),
            "f.txt-8-l8\nf.txt-9-l9\nf.txt:10:l10\n"
        );
        // No context means plain match rows with no separators.
        assert_eq!(
            grep(&ws, json!({"pattern": "l[27]$"})),
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
            two,
            "a.txt-1-l1\na.txt:2:l2\na.txt-3-l3\n--\nb.txt-1-l1\nb.txt:2:l2\nb.txt-3-l3\n"
        );
        // max_results limits matches, not the context rows around them.
        let one = grep(
            &ws,
            json!({"pattern": "l2$", "context": 1, "max_results": 1}),
        );
        assert!(
            one.starts_with("a.txt-1-l1\na.txt:2:l2\na.txt-3-l3\n"),
            "{one}"
        );
        assert!(
            !one.contains("b.txt") && one.contains("stopped at 1 matches"),
            "{one}"
        );
    }

    #[test]
    fn grep_caps_matches_and_bounds_each_line() {
        let (dir, ws) = workspace();
        let many: String = (0..1000).map(|_| "hit\n").collect();
        fs::write(dir.path().join("many.log"), many).unwrap();
        let capped = grep(&ws, json!({"pattern": "hit", "path": "many.log"}));
        assert_eq!(capped.matches("many.log:").count(), DEFAULT_GREP_MATCHES);
        assert!(capped.contains("stopped at 200 matches"));
        let five = grep(
            &ws,
            json!({"pattern": "hit", "path": "many.log", "max_results": 5}),
        );
        assert_eq!(five.matches("many.log:").count(), 5);
        let raised = grep(
            &ws,
            json!({"pattern": "hit", "path": "many.log", "max_results": 5000}),
        );
        assert_eq!(
            raised.matches("many.log:").count(),
            1000,
            "an oversized max_results is clamped, not refused"
        );
        fs::write(
            dir.path().join("long.txt"),
            format!("needle{}\n", "z".repeat(5000)),
        )
        .unwrap();
        let long = grep(&ws, json!({"pattern": "needle", "path": "long.txt"}));
        assert!(
            long.len() < 400 && long.contains("…[truncated]"),
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
        assert!(out.len() < 600, "{}", out.len());
        // A match near the start keeps the plain prefix.
        fs::write(
            dir.path().join("short.js"),
            format!("NEEDLE_HERE{}\n", "z".repeat(1000)),
        )
        .unwrap();
        let near = grep_tool(&ws, &json!({"pattern": "NEEDLE_HERE", "path": "short.js"})).unwrap();
        assert!(
            near.contains("short.js:1:NEEDLE_HERE") && !near.contains("skipped"),
            "{near}"
        );
    }

    #[test]
    fn capped_glob_keeps_lexical_paths_not_depth_first_paths() {
        let (dir, ws) = workspace();
        fs::create_dir(dir.path().join("a")).unwrap();
        let mut names = vec!["a.rs".to_string()];
        fs::write(dir.path().join("a.rs"), "").unwrap();
        for index in 0..MAX_LIST + 3 {
            let name = format!("a/{index:04}.rs");
            fs::write(dir.path().join(&name), "").unwrap();
            names.push(name);
        }
        names.sort();
        let expected = format!(
            "{}\n[4 more matches not shown; narrow the pattern]\n",
            names[..MAX_LIST].join("\n")
        );
        for _ in 0..3 {
            assert_eq!(
                glob_tool(&ws, &json!({"pattern": "**/*.rs"})).unwrap(),
                expected
            );
        }
    }

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
                glob_tool(&ws, &json!({"pattern": format!("{prefix}/**/*.rs")})).unwrap(),
                "no matches"
            );
        }
        assert_eq!(
            glob_tool(&ws, &json!({"pattern": "src/**/mod.rs"})).unwrap(),
            "src/deep/mod.rs\n"
        );
        assert_eq!(
            glob_tool(&ws, &json!({"pattern": "{src,docs}/**/mod.rs"})).unwrap(),
            "docs/mod.rs\nsrc/deep/mod.rs\n"
        );
        assert_eq!(
            grep(&ws, json!({"pattern": "needle", "glob": "src/**/mod.rs"})),
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
        let expected = "b.txt:2:needle\nc.txt:1:needle\n[1 binary or oversized files skipped]\n";
        assert_eq!(grep(&ws, json!({"pattern": "needle"})), expected);
    }

    #[test]
    fn grep_cap_preserves_deterministic_depth_first_order() {
        let (dir, ws) = workspace();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/z.rs"), "hit first\nhit second\n").unwrap();
        fs::write(dir.path().join("a.rs"), "hit later\n").unwrap();
        let expected = "a/z.rs:1:hit first\n[stopped at 1 matches (max_results 1) or 20 KiB; narrow with path, glob or a more specific pattern]\n";
        for _ in 0..3 {
            assert_eq!(
                grep(&ws, json!({"pattern": "hit", "max_results": 1})),
                expected
            );
        }
    }
}

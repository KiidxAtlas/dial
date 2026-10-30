//! Bash intent interceptor: points shell commands that duplicate a dedicated tool at that tool.
//!
//! In the default `hint` mode the command runs and its result gains one line naming the tool;
//! only a command that is nothing but the duplicate (optionally trimmed by `head`/`tail`) is
//! hinted, so batched compound commands, which save model round trips, pass untouched. `block`
//! mode refuses any command with a matching segment instead. [`ci_poll_hint`] separately steers
//! repeated `gh run` status checks toward one blocking wait.
//!
//! Rules are written as JavaScript `RegExp` patterns. Dial uses `fancy-regex` (lookaround
//! support, backtracking like JS). Rule patterns are JS syntax, so [`translate_pattern`]
//! rewrites the JS-only semantics that differ in Rust syntax: ASCII `\w`/`\d`, the JS
//! `\s` set, `.` excluding `\r`/`\u{2028}`/`\u{2029}`, and the `m`/`s`/`y` flags.
use crate::config::tools::bash_interceptor::BashInterceptorConfig;
use crate::config::tools::bash_interceptor::InterceptMode;
use crate::config::tools::bash_interceptor::Rule;
use crate::config::tools::bash_interceptor::default_rules;

use std::sync::LazyLock;

use fancy_regex::Regex;

/// The interceptor's decision for one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Do not run the command; the text is the tool error.
    Block(String),
    /// Run the command and append this line to its output.
    Hint(String),
}

/// Decides what happens to `command` when a dedicated tool in `available_tools` covers it;
/// `None` runs it unchanged.
///
/// `block` checks the trimmed command, each flat segment not fed by a pipe, and that segment
/// without leading `NAME=value` assignments. `hint` checks only a command that is a single
/// duplicate: one segment, optionally after a leading `cd DIR` and before `| head`/`| tail`
/// stages. Rules run in order; the first match wins.
pub fn check(
    config: &BashInterceptorConfig,
    available_tools: &[&str],
    command: &str,
) -> Option<Verdict> {
    if !config.enabled {
        return None;
    }
    let candidates = match config.mode {
        InterceptMode::Block => interception_candidates(command),
        InterceptMode::Hint => hint_candidates(command),
    };
    if candidates.is_empty() {
        return None;
    }
    let custom: Vec<Compiled>;
    let rules: &[Compiled] = match &config.patterns {
        None => DEFAULT_COMPILED.as_slice(),
        Some(patterns) => {
            custom = compile_rules(patterns);
            &custom
        }
    };
    for compiled in rules {
        if !available_tools.contains(&compiled.tool.as_str()) {
            continue;
        }
        for candidate in &candidates {
            if compiled.matches(candidate) {
                return Some(match config.mode {
                    InterceptMode::Block => Verdict::Block(format!(
                        "Blocked: {}\n\nOriginal command: {command}",
                        compiled.message
                    )),
                    InterceptMode::Hint => Verdict::Hint(format!("[hint] {}", compiled.message)),
                });
            }
        }
    }
    None
}

/// Stages that reduce their input; a `head`/`tail` after one selects a result (top-N), not a trim.
const REDUCERS: &[&str] = &[
    "sort", "uniq", "wc", "awk", "sed", "cut", "tr", "xargs", "jq",
];

/// One line for a command whose last pipe stage is `| head` / `| tail` over a plain command:
/// `bash` already bounds long output (keeping its start and end), so the trim only hides lines
/// that the model then reruns the command to see, which for builds and linters repeats the
/// full run. Pipelines with a reducing stage (`sort | uniq -c | head`) select and are left alone.
pub fn trim_hint(config: &BashInterceptorConfig, command: &str) -> Option<String> {
    if !config.enabled {
        return None;
    }
    let segments = extract_flat_shell_command_segments(command);
    let trims = segments.iter().enumerate().any(|(index, stage)| {
        let ends_pipeline = segments.get(index + 1).is_none_or(|next| !next.piped_stdin);
        if !(stage.piped_stdin && is_trim(&stage.text) && ends_pipeline) {
            return false;
        }
        let start = segments[..index]
            .iter()
            .rposition(|s| !s.piped_stdin)
            .unwrap_or(0);
        !segments[start..index].iter().any(|upstream| {
            upstream
                .text
                .split_whitespace()
                .next()
                .is_some_and(|word| REDUCERS.contains(&word))
        })
    });
    trims.then(|| {
        "[hint] `bash` already bounds long output (it keeps the start and end), so `| head`/`| tail` only hides lines you may rerun the command to see. Drop the trim, and put independent commands in one response instead of one call each.".to_owned()
    })
}

/// The command at the head of each flat pipeline in `command`, with whether a later stage of that
/// pipeline filters its output (`| tail`, `| grep`, ...). A filtered run keeps only what the
/// filter let through, so it cannot stand in for the command's full output. `2>&1` is dropped
/// from the head so `make lint` and `make lint 2>&1 | tail` name the same command. Empty when
/// the syntax cannot be scanned flatly (substitution, heredocs, grouping).
pub fn pipeline_heads(command: &str) -> Vec<(String, bool)> {
    let segments = extract_flat_shell_command_segments(command);
    let mut heads: Vec<(String, bool)> = Vec::new();
    for segment in &segments {
        if segment.piped_stdin {
            if let Some(last) = heads.last_mut() {
                last.1 = true;
            }
        } else {
            let text = segment.text.replace(" 2>&1", "");
            heads.push((text.trim().to_owned(), false));
        }
    }
    heads
}

/// The only command of `command` when it is a single flat pipeline stage, without leading
/// `NAME=value` assignments or `2>&1`. `None` for compound commands (`a && b`, `a; b`), pipelines
/// (the stage's output is not the command's), and syntax the flat scanner declines.
pub fn sole_command(command: &str) -> Option<String> {
    let mut heads = pipeline_heads(command);
    let (head, filtered) = heads.pop().filter(|_| heads.is_empty())?;
    if filtered {
        return None;
    }
    Some(without_leading_environment_assignments(&head).unwrap_or(head))
}

/// `gh` commands that read a workflow run's or a PR's check status.
static CI_STATUS_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bgh\s+(?:run\s+(?:view|list)|pr\s+checks)\b").expect("valid CI status regex")
});
/// A shell loop that sleeps between status checks.
static SLEEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsleep\s+\d").expect("valid sleep regex"));
/// Statuses GitHub reports for a run or check that has not finished.
static UNFINISHED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:in_progress|queued|pending|waiting|requested)\b")
        .expect("valid unfinished-status regex")
});

/// One line steering a CI status poll toward a single blocking wait: a `gh run view|list` or
/// `gh pr checks` whose output still shows an unfinished run, or one that sleeps before or
/// between checks without already using `gh run watch`. Each repeated poll costs a full model
/// request; a wait costs one.
pub fn ci_poll_hint(
    config: &BashInterceptorConfig,
    available_tools: &[&str],
    command: &str,
    output: &str,
) -> Option<String> {
    if !config.enabled
        || !CI_STATUS_COMMAND.is_match(command).unwrap_or(false)
        || command.contains("--log")
    {
        return None;
    }
    let sleeps = SLEEP.is_match(command).unwrap_or(false) && !command.contains("gh run watch");
    if !sleeps && !UNFINISHED.is_match(output).unwrap_or(false) {
        return None;
    }
    Some(if available_tools.contains(&"github") {
        "[hint] To wait for CI, call `github` op `run_watch` once (it returns when the run ends, with failed-job logs) instead of polling `gh run`.".to_owned()
    } else if available_tools.contains(&"bg") {
        "[hint] To wait for CI, run `gh run watch <id> --exit-status > /dev/null` once: it moves to the background, you are notified when the run ends, and `bg` action `wait` blocks on it. Do not poll `gh run`.".to_owned()
    } else {
        "[hint] To wait for CI, run `gh run watch <id> --exit-status > /dev/null` once instead of polling `gh run`.".to_owned()
    })
}

struct Compiled {
    tool: String,
    message: String,
    regex: Regex,
}

impl Compiled {
    fn matches(&self, candidate: &str) -> bool {
        // A runtime failure (fancy-regex backtrack limit) cannot be evaluated; treat it as a
        // match so pathological input is not silently allowed past a rule.
        self.regex.is_match(candidate).unwrap_or(true)
    }
}

/// Compiles rules, skipping invalid patterns or flags.
fn compile_rules(rules: &[Rule]) -> Vec<Compiled> {
    rules
        .iter()
        .filter_map(|rule| {
            Some(Compiled {
                tool: rule.tool.clone(),
                message: rule.message.clone(),
                regex: compile_rule(rule)?,
            })
        })
        .collect()
}

static DEFAULT_COMPILED: LazyLock<Vec<Compiled>> =
    LazyLock::new(|| compile_rules(&default_rules()));

/// Compiles one rule with JS `RegExp` flag semantics. `None` for invalid flags or patterns.
fn compile_rule(rule: &Rule) -> Option<Regex> {
    let flags = rule.flags.as_deref().unwrap_or("");
    let mut seen = String::new();
    for flag in flags.chars() {
        if !"dgimsuvy".contains(flag) || seen.contains(flag) {
            return None;
        }
        seen.push(flag);
    }
    if seen.contains('u') && seen.contains('v') {
        return None;
    }
    let has = |flag: char| seen.contains(flag);
    let mut body = translate_pattern(&rule.pattern, has('s'), has('m'))?;
    if has('y') {
        // `test` with lastIndex reset to 0 matches only at the start of the input.
        body = format!(r"\A(?:{body})");
    }
    let mut prefix = String::new();
    if has('i') {
        prefix.push_str("(?i)");
    }
    if has('s') {
        prefix.push_str("(?s)");
    }
    Regex::new(&format!("{prefix}{body}")).ok()
}

/// JS `\s` set (ECMAScript WhiteSpace + LineTerminator), as a bracket-class body.
const SPACE_SET: &str = r"\t\n\x0B\x0C\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}";
/// JS `\w` set (ASCII only without the `u` flag).
const WORD_SET: &str = "0-9A-Za-z_";
const DIGIT_SET: &str = "0-9";
/// LineTerminator characters: `.` excludes these, and `^`/`$` match next to them with `m`.
const LINE_TERMINATORS: &str = r"\n\r\x{2028}\x{2029}";
/// Characters that carry meaning in Rust/fancy-regex syntax; other escaped punctuation is a
/// literal in both dialects and is emitted bare.
const RUST_META: &str = r"\.+*?()|[]{}^$#&-~";

fn shorthand(escape: char) -> Option<(&'static str, bool)> {
    Some(match escape {
        's' => (SPACE_SET, false),
        'S' => (SPACE_SET, true),
        'w' => (WORD_SET, false),
        'W' => (WORD_SET, true),
        'd' => (DIGIT_SET, false),
        'D' => (DIGIT_SET, true),
        _ => return None,
    })
}

/// Rewrites a JS (non-`u`) pattern into fancy-regex syntax with JS semantics. `None` when the
/// pattern is malformed in a way JS would reject (trailing backslash, unterminated class).
fn translate_pattern(pattern: &str, dot_all: bool, multiline: bool) -> Option<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() * 2);
    let mut in_class = false;
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        i += 1;
        if ch == '\\' {
            let escape = *chars.get(i)?;
            i += 1;
            if let Some((set, negated)) = shorthand(escape) {
                // Positive sets inline into an enclosing class; everything else is a nested class.
                if in_class && !negated {
                    out.push_str(set);
                } else {
                    out.push('[');
                    if negated {
                        out.push('^');
                    }
                    out.push_str(set);
                    out.push(']');
                }
            } else if escape == 'b' && in_class {
                out.push_str(r"\x08");
            } else if !in_class && (escape == 'b' || escape == 'B') {
                let w = format!("[{WORD_SET}]");
                if escape == 'b' {
                    out.push_str(&format!("(?:(?<={w})(?!{w})|(?<!{w})(?={w}))"));
                } else {
                    out.push_str(&format!("(?:(?<={w})(?={w})|(?<!{w})(?!{w}))"));
                }
            } else if matches!(escape, 'n' | 'r' | 't' | 'v' | 'f' | 'x' | 'u')
                || escape.is_ascii_digit()
            {
                out.push('\\');
                out.push(escape);
            } else if escape.is_ascii_alphabetic() {
                // JS identity escape (e.g. `\p` is a plain `p` without `u`); fancy rejects these.
                out.push(escape);
            } else if RUST_META.contains(escape) {
                out.push('\\');
                out.push(escape);
            } else {
                out.push(escape);
            }
            continue;
        }
        if in_class {
            match ch {
                ']' => {
                    in_class = false;
                    out.push(']');
                }
                '[' | '&' | '~' => {
                    out.push('\\');
                    out.push(ch);
                }
                _ => out.push(ch),
            }
            continue;
        }
        match ch {
            '[' => {
                in_class = true;
                out.push('[');
            }
            '.' if dot_all => out.push('.'),
            '.' => out.push_str(&format!("[^{LINE_TERMINATORS}]")),
            '^' if multiline => out.push_str(&format!("(?:^|(?<=[{LINE_TERMINATORS}]))")),
            '$' if multiline => out.push_str(&format!("(?:$|(?=[{LINE_TERMINATORS}]))")),
            _ => out.push(ch),
        }
    }
    if in_class {
        return None;
    }
    Some(out)
}

/// The command and each segment that does not read piped stdin, checked against the rules.
fn interception_candidates(command: &str) -> Vec<String> {
    let mut candidates = vec![command.trim().to_owned()];
    for segment in extract_flat_shell_command_segments(command) {
        // A segment reading piped stdin cannot be replaced by a path-based tool; only standalone
        // and first-stage commands are candidates.
        if segment.piped_stdin {
            continue;
        }
        candidates.push(segment.text.clone());
        if let Some(without) = without_leading_environment_assignments(&segment.text)
            && !without.is_empty()
        {
            candidates.push(without);
        }
    }
    candidates
}

/// Candidates for `hint` mode: empty unless the command is one duplicate. A leading `cd DIR`
/// segment is skipped and trailing `| head` / `| tail` stages are allowed, since the dedicated
/// tools take a directory and limit their own output. Anything else (a second command, a pipe
/// into another program, syntax the flat scanner declines) is deliberate shell work.
fn hint_candidates(command: &str) -> Vec<String> {
    let mut segments = extract_flat_shell_command_segments(command);
    if segments.len() > 1 && !segments[1].piped_stdin && is_cd(&segments[0].text) {
        segments.remove(0);
    }
    let Some((head, rest)) = segments.split_first() else {
        return Vec::new();
    };
    if head.piped_stdin
        || !rest
            .iter()
            .all(|stage| stage.piped_stdin && is_trim(&stage.text))
    {
        return Vec::new();
    }
    let mut candidates = vec![head.text.clone()];
    if let Some(without) = without_leading_environment_assignments(&head.text) {
        candidates.push(without);
    }
    if rest.is_empty() {
        // Background syntax (`cmd &`) is consumed by the segment split; keep the whole command.
        candidates.push(command.trim().to_owned());
    }
    candidates
}

fn is_cd(segment: &str) -> bool {
    segment == "cd" || segment.starts_with("cd ") || segment.starts_with("cd\t")
}

/// `head`/`tail` stages that only trim output (not `tail -f`).
fn is_trim(stage: &str) -> bool {
    let mut words = stage.split_whitespace();
    matches!(words.next(), Some("head" | "tail"))
        && words.all(|word| !matches!(word, "-f" | "-F" | "--follow"))
}

/// One flat command segment with whether its stdin is fed by a preceding `|` / `|&`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FlatSegment {
    text: String,
    piped_stdin: bool,
}

/// Pushes the trimmed `chars[start..end]` as a segment when non-empty. Returns whether pushed.
fn push_segment(
    segments: &mut Vec<FlatSegment>,
    chars: &[char],
    start: usize,
    end: usize,
    piped: bool,
) -> bool {
    if start >= end {
        return false;
    }
    let text: String = chars[start..end].iter().collect();
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    segments.push(FlatSegment {
        text: text.to_owned(),
        piped_stdin: piped,
    });
    true
}

/// Splits a conservative flat command on `\n`, `;`,
/// `|`, `|&`, `&`, `&&`, `||`, honouring quotes, escapes, and comments. Returns an empty list
/// whenever the syntax cannot be scanned safely (heredocs, substitution, grouping, unterminated
/// quotes); callers still check the whole command in that case.
fn extract_flat_shell_command_segments(command: &str) -> Vec<FlatSegment> {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut segments = Vec::new();
    let mut segment_start = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut at_word_start = true;
    let mut current_piped = false;

    let mut i = 0usize;
    while i < n {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if ch == '\\' {
                if i + 1 >= n {
                    return Vec::new();
                }
                i += 2;
                continue;
            }
            if ch == '"' {
                in_double = false;
                i += 1;
                continue;
            }
            if ch == '`' || (ch == '$' && next == Some('(')) {
                return Vec::new();
            }
            i += 1;
            continue;
        }

        if ch == '\'' {
            in_single = true;
            at_word_start = false;
            i += 1;
            continue;
        }
        if ch == '"' {
            in_double = true;
            at_word_start = false;
            i += 1;
            continue;
        }
        if ch == '\\' {
            if i + 1 >= n {
                return Vec::new();
            }
            at_word_start = false;
            i += 2;
            continue;
        }
        let brace_word = (ch == '{' || ch == '}')
            && at_word_start
            && matches!(next, None | Some(' ' | '\t' | '\n' | ';'));
        if ch == '`'
            || ch == '('
            || ch == ')'
            || (ch == '$' && matches!(next, Some('(' | '{')))
            || (ch == '<' && next == Some('<'))
            || brace_word
        {
            return Vec::new();
        }
        if ch == '#' && at_word_start {
            let pushed = push_segment(&mut segments, &chars, segment_start, i, current_piped);
            let Some(newline) = chars[i + 1..]
                .iter()
                .position(|&c| c == '\n')
                .map(|p| p + i + 1)
            else {
                return segments;
            };
            i = newline + 1;
            segment_start = newline + 1;
            at_word_start = true;
            // Preserve a pending pipe through a comment-only continuation.
            if pushed {
                current_piped = false;
            }
            continue;
        }
        let is_redirect_operator = match ch {
            '|' => i > 0 && chars[i - 1] == '>',
            '&' => (i > 0 && matches!(chars[i - 1], '>' | '<')) || next == Some('>'),
            _ => false,
        };
        if matches!(ch, '\n' | ';' | '|' | '&') && !is_redirect_operator {
            let pushed = push_segment(&mut segments, &chars, segment_start, i, current_piped);
            let doubled = matches!(ch, '|' | '&') && next == Some(ch);
            let pipe_stderr = ch == '|' && next == Some('&');
            if doubled || pipe_stderr {
                i += 1;
            }
            // `|` and `|&` pipe into the next segment. Blank continuation lines preserve that
            // pending state; all other operators reset it.
            if pushed || ch != '\n' {
                current_piped = ch == '|' && !doubled;
            }
            segment_start = i + 1;
            at_word_start = true;
            i += 1;
            continue;
        }
        at_word_start = ch == ' ' || ch == '\t';
        i += 1;
    }

    if in_single || in_double {
        return Vec::new();
    }
    push_segment(&mut segments, &chars, segment_start, n, current_piped);
    segments
}

/// Flat sequential commands for output classification. Reject pipes and asynchronous jobs: their
/// interleaved output cannot be attributed to a recognized producer.
pub(super) fn output_commands(command: &str) -> Option<Vec<String>> {
    if command.replace("&&", "").contains('&') {
        return None;
    }
    let segments = extract_flat_shell_command_segments(command);
    if segments.is_empty() || segments.iter().any(|segment| segment.piped_stdin) {
        return None;
    }
    Some(segments.into_iter().map(|segment| segment.text).collect())
}

/// Finds the end of a shell word, respecting quotes and escapes. `None` for incomplete syntax.
/// Returns the index of the terminating space/tab, or the length when the word runs to the end.
fn skip_shell_word(chars: &[char], start: usize) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let mut i = start;
    while i < chars.len() {
        let ch = chars[i];
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if ch == '\\' {
                if i + 1 >= chars.len() {
                    return None;
                }
                i += 2;
                continue;
            }
            if ch == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '\\' => {
                if i + 1 >= chars.len() {
                    return None;
                }
                i += 2;
                continue;
            }
            ' ' | '\t' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    if in_single || in_double {
        None
    } else {
        Some(chars.len())
    }
}

/// Removes leading `NAME=value` assignments without interpreting shell syntax.
fn without_leading_environment_assignments(command: &str) -> Option<String> {
    let chars: Vec<char> = command.chars().collect();
    let n = chars.len();
    let mut index = 0usize;
    let mut found_assignment = false;
    while index < n {
        while index < n && matches!(chars[index], ' ' | '\t') {
            index += 1;
        }
        let assignment_start = index;
        if !(index < n && (chars[index].is_ascii_alphabetic() || chars[index] == '_')) {
            break;
        }
        let mut name_end = index + 1;
        while name_end < n && (chars[name_end].is_ascii_alphanumeric() || chars[name_end] == '_') {
            name_end += 1;
        }
        if chars.get(name_end) != Some(&'=') {
            // Returns the remainder even when empty; the caller drops empty results.
            return if found_assignment {
                let rest: String = chars[assignment_start..].iter().collect();
                Some(rest.trim_start().to_owned())
            } else {
                None
            };
        }
        let word_end = skip_shell_word(&chars, name_end + 1)?;
        found_assignment = true;
        index = word_end;
        if index == n {
            return None;
        }
    }
    if !found_assignment {
        return None;
    }
    let rest: String = chars[index..].iter().collect();
    let rest = rest.trim_start();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

#[cfg(test)]
mod tests;

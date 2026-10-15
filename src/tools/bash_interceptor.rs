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

use std::sync::LazyLock;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};

/// One bash-interceptor rule: commands matching `pattern` are redirected to `tool`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<String>,
    pub tool: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_subcommands: Option<Vec<String>>,
}

/// What a matching command gets.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InterceptMode {
    /// Run the command and append a one-line pointer to the dedicated tool.
    #[default]
    Hint,
    /// Refuse the command with the rule's message; the model must reissue it.
    Block,
}

/// `[bash_interceptor]` configuration. `patterns: None` selects [`default_rules`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BashInterceptorConfig {
    pub enabled: bool,
    pub mode: InterceptMode,
    pub patterns: Option<Vec<Rule>>,
}

impl Default for BashInterceptorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: InterceptMode::Hint,
            patterns: None,
        }
    }
}

/// The interceptor's decision for one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Do not run the command; the text is the tool error.
    Block(String),
    /// Run the command and append this line to its output.
    Hint(String),
}

fn rule(pattern: &str, tool: &str, message: &str) -> Rule {
    Rule {
        pattern: pattern.to_owned(),
        flags: None,
        tool: tool.to_owned(),
        message: message.to_owned(),
        allow_subcommands: None,
    }
}

/// Built-in interception rules, in priority order (first match wins).
pub fn default_rules() -> Vec<Rule> {
    vec![
        // `cat >file` / `cat <<EOF` write rather than read and fall through to the write rule;
        // `tail -f` follows a live file, which `read` cannot, and falls through to the bg rule.
        rule(
            r"^\s*(?:cat(?!\s*(?:>|<<))|head|tail(?!\s+-[fF](?:\s|$))|less|more)\s+",
            "read",
            "Use the `read` tool instead of cat/head/tail. It provides better context and handles binary files.",
        ),
        // Only a bare line-range print with a file operand; `sed -n '/re/p'`, multi-command
        // scripts and stdin filters are left alone.
        rule(
            r#"^\s*sed\s+-n\s+(?:'\d+(?:,\d+)?p'|"\d+(?:,\d+)?p"|\d+(?:,\d+)?p)\s+[^\s|;&<>]+\s*$"#,
            "read",
            "Use the `read` tool with offset/limit instead of sed -n 'A,Bp'.",
        ),
        rule(
            r"^\s*(grep|rg|ripgrep|ag|ack)\s+",
            "grep",
            "Use the `grep` tool instead of grep/rg. It respects .gitignore and provides structured output.",
        ),
        // Predicates and actions a glob cannot express (`-exec`, `-delete`, times, sizes,
        // permissions, custom output) keep `find`.
        rule(
            r"^\s*(?!.*\s-(?:exec|execdir|ok|okdir|delete|newer|[acm]min|[acm]time|size|perm|user|group|links|empty|printf|fprint|print0)\b)(find|fd|locate)\s+.*(-name|-iname|-type|--type|-glob)",
            "glob",
            "Use the `glob` tool instead of find/fd. It respects .gitignore and is faster for glob patterns.",
        ),
        rule(
            r"^\s*sed\s+(-i|--in-place)",
            "edit",
            "Use the `edit` tool instead of sed -i. It provides diff preview, and /undo can restore it.",
        ),
        // `-i` must be one of perl's leading switches; an `-i` inside the program text is not.
        rule(
            r"^\s*perl(?:\s+-[A-Za-z]+)*?\s+-[A-Za-z]*i[A-Za-z.]*(?:\s|$)",
            "edit",
            "Use the `edit` tool instead of perl -i. It provides diff preview, and /undo can restore it.",
        ),
        rule(
            r"^\s*awk\s+.*-i\s+inplace",
            "edit",
            "Use the `edit` tool instead of awk -i inplace. It provides diff preview, and /undo can restore it.",
        ),
        // `>` must sit outside quoted regions (so `echo "a -> b"` passes) and be followed by a
        // plausible filename, including `$VAR` targets; `>|` (clobber) counts as a redirect;
        // `>&2`/`2>&1` style fd duplication is not matched. Allowed device sinks are consumed
        // while looking for later real file redirects because the write tool cannot replace
        // shell output/discard targets.
        rule(
            r#"^\s*(echo|printf|cat\s*<<)\s+(?:(?:[^"'>]|"[^"]*"|'[^']*')|(?<!\|)>{1,2}\|?\s*(?:"/dev/(?:null|tty|stdout|stderr)"|'/dev/(?:null|tty|stdout|stderr)'|/dev/(?:null|tty|stdout|stderr))(?:[\s;&|]|$))*(?<!\|)>{1,2}\|?\s*(?!(?:"/dev/(?:null|tty|stdout|stderr)"|'/dev/(?:null|tty|stdout|stderr)'|/dev/(?:null|tty|stdout|stderr))(?:[\s;&|]|$))[$\w./~"'-]"#,
            "write",
            "Use the `write` tool instead of echo/cat redirection. It handles encoding and provides confirmation.",
        ),
        rule(
            r"^\s*nohup\s+|(?<!&)&\s*$",
            "bg",
            "Use `bg` with `action: start` and `name` instead of nohup or a trailing `&`, so the process stays observable and managed.",
        ),
        rule(
            r"^\s*(?:(?:bun|npm|pnpm|yarn)\s+(?:run\s+)?(?:dev|start)(?:\s|$)|(?:vite|next\s+dev|nuxt\s+dev|nodemon|lldb|gdb|tail\s+-[fF])(?:\s|$)|docker\s+compose\s+up(?!.*(?:\s-d(?:\s|$)|--detach))(?:\s|$))",
            "bg",
            "Use `bg` with `action: start` and `name` for services, watchers, and debuggers; inspect with `bg` action `output` or `wait`.",
        ),
        rule(
            r"^\s*(?:(?:(?:bun|npm|pnpm|yarn)\s+(?:run\s+)?\S+|jest)(?:.|\n)*--watch|(?:cargo\s+watch|watchexec|pytest|vitest|tsc)(?:.|\n)*(?:--watch|-w))(?:\s|$)",
            "bg",
            "Use `bg` with `action: start` and `name` for watch mode so its output, input, and lifecycle stay managed.",
        ),
    ]
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
        if let Some(without) = without_leading_environment_assignments(&segment.text) {
            if !without.is_empty() {
                candidates.push(without);
            }
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
mod tests {
    use super::*;

    const TOOLS: &[&str] = &["read", "grep", "glob", "edit", "write", "bash", "bg"];

    fn on() -> BashInterceptorConfig {
        BashInterceptorConfig {
            enabled: true,
            mode: InterceptMode::Block,
            patterns: None,
        }
    }

    fn hint_mode() -> BashInterceptorConfig {
        BashInterceptorConfig::default()
    }

    fn blocked(config: &BashInterceptorConfig, command: &str) -> Option<String> {
        match check(config, TOOLS, command)? {
            Verdict::Block(message) => Some(message),
            Verdict::Hint(line) => panic!("expected a block, got {line}"),
        }
    }

    /// Tool of the default rule whose hint `command` gets.
    fn hinted_tool(command: &str) -> Option<String> {
        let Verdict::Hint(line) = check(&hint_mode(), TOOLS, command)? else {
            panic!("expected a hint for {command}");
        };
        default_rules()
            .into_iter()
            .find(|r| line == format!("[hint] {}", r.message))
            .map(|r| r.tool)
    }

    /// Tool of the default rule that blocked `command`, exactly matched by message.
    fn blocked_tool_with(
        config: &BashInterceptorConfig,
        tools: &[&str],
        command: &str,
    ) -> Option<String> {
        let Verdict::Block(message) = check(config, tools, command)? else {
            panic!("expected a block for {command}");
        };
        default_rules()
            .into_iter()
            .find(|r| message == format!("Blocked: {}\n\nOriginal command: {command}", r.message))
            .map(|r| r.tool)
    }

    fn blocked_tool(command: &str) -> Option<String> {
        blocked_tool_with(&on(), TOOLS, command)
    }

    fn segments(command: &str) -> Vec<(String, bool)> {
        extract_flat_shell_command_segments(command)
            .into_iter()
            .map(|s| (s.text, s.piped_stdin))
            .collect()
    }

    #[test]
    fn default_rules_all_compile_and_match_count() {
        let rules = default_rules();
        assert_eq!(rules.len(), 11);
        for r in &rules {
            assert!(
                compile_rule(r).is_some(),
                "rule failed to compile: {}",
                r.pattern
            );
        }
    }

    #[test]
    fn disabled_config_returns_none() {
        for mode in [InterceptMode::Hint, InterceptMode::Block] {
            let off = BashInterceptorConfig {
                enabled: false,
                mode,
                patterns: None,
            };
            assert_eq!(check(&off, TOOLS, "cat x"), None);
            assert_eq!(
                ci_poll_hint(&off, TOOLS, "gh run view 1", "run: in_progress"),
                None
            );
        }
    }

    #[test]
    fn default_is_enabled_hint_mode() {
        let config = BashInterceptorConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, InterceptMode::Hint);
        let parsed: BashInterceptorConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, config);
        let block: BashInterceptorConfig = serde_json::from_str(r#"{"mode":"block"}"#).unwrap();
        assert_eq!(block.mode, InterceptMode::Block);
        assert!(serde_json::from_str::<BashInterceptorConfig>(r#"{"mode":"warn"}"#).is_err());
    }

    #[test]
    fn read_rule() {
        for c in [
            "cat notes.txt",
            "  head -n 5 a.rs",
            "tail -n 5 log",
            "less README",
            "more x",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("read"), "{c}");
        }
        for c in ["catalog x", "cat", "echo cat x", "bat x"] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
        // Writing through `cat` is not a read.
        assert_eq!(blocked_tool("cat > notes.md"), None);
        assert_eq!(blocked_tool("cat >> notes.md"), None);
    }

    #[test]
    fn sed_line_range_print_is_read() {
        for c in [
            "sed -n 10,20p src/main.rs",
            "sed -n '1,40p' README.md",
            "sed -n \"5p\" a.txt",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("read"), "{c}");
        }
        for c in [
            "sed -n '/fn main/,/^}/p' src/main.rs",
            "sed -n 1,5p",
            "sed -n '1,40p' a.rs b.rs",
            "sed 's/a/b/' f",
        ] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn grep_rule() {
        for c in [
            "grep -r foo .",
            "rg foo",
            "ripgrep foo",
            "ag foo",
            "ack foo",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("grep"), "{c}");
        }
        for c in ["rgb foo", "egrep x"] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn glob_rule() {
        for c in [
            "find . -name '*.rs'",
            "fd --type f foo",
            "locate -iname x",
            "find . -maxdepth 1 -type f",
            "find . -glob '*.md'",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("glob"), "{c}");
        }
        for c in [
            "find . -print",
            "fdisk -l",
            "findstr x",
            "find . -name '*.pyc' -delete",
            "find src -name '*.rs' -exec wc -l {} +",
            "find . -type f -mtime -1",
            "find . -type f -newer Cargo.lock",
            "find . -name '*.log' -size +1M",
            "find . -type f -print0",
        ] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn sed_perl_awk_rules() {
        for c in ["sed -i 's/a/b/' f", "sed --in-place=.bak s/a/b/ f"] {
            assert_eq!(blocked_tool(c).as_deref(), Some("edit"), "{c}");
        }
        for c in ["sed 's/a/b/' f", "sed -n '/x/p' f"] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
        for c in [
            "perl -pi -e 's/a/b/' f",
            "perl -i -pe 's/a/b/' f",
            "perl -ni -e x f",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("edit"), "{c}");
        }
        for c in ["perl -i.bak -pe 's/a/b/' f", "perl -w -i -pe x f"] {
            assert_eq!(blocked_tool(c).as_deref(), Some("edit"), "{c}");
        }
        for c in [
            "perl -e 'print 1'",
            "perl script.pl",
            "perl -ne 'print if /x-id/' f",
            "perl -pe 's/-i//' f",
            "perl -I lib t/run.pl",
        ] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
        assert_eq!(
            blocked_tool("awk -i inplace '{print}' f").as_deref(),
            Some("edit")
        );
        for c in ["awk '{print}' f", "awk -F: '{print $1}' f"] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn redirect_rule_positive() {
        for c in [
            "echo hi > out.txt",
            "echo hi >> notes",
            "printf 'x' > file.txt",
            "echo hi >| out",
            "echo hi > $HOME/x",
            "echo hi > ~/x",
            "echo hi > /dev/null; echo hi > out.txt",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("write"), "{c}");
        }
        // A heredoc into a file is a write, not a read.
        assert_eq!(
            blocked_tool("cat << 'EOF' > notes.md").as_deref(),
            Some("write")
        );
    }

    #[test]
    fn redirect_rule_negative_and_dev_null_exemption() {
        for c in [
            "echo \"a -> b\"",
            "echo hi > /dev/null",
            "echo hi 2>/dev/null",
            "echo hi > \"/dev/null\"",
            "echo hi > /dev/stderr",
            "echo hi >/dev/tty",
            "echo x >&2",
            "echo hi > /dev/null && echo bye",
            "echo a && b",
        ] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn redirect_target_uses_js_ascii_word_class() {
        // JS `\w` is ASCII-only without the `u` flag, so a non-ASCII target is not a redirect.
        assert_eq!(blocked_tool("echo hi > é.txt"), None);
        assert_eq!(blocked_tool("echo hi > ébc"), None);
    }

    #[test]
    fn nohup_and_background_rule() {
        for c in ["nohup node server.js", "node server.js &", "sleep 100 &"] {
            assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
        }
        for c in ["make && test", "echo a && b"] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn service_rule() {
        for c in [
            "npm run dev",
            "bun dev",
            "pnpm start",
            "yarn dev --port 3000",
            "vite",
            "vite --port 3000",
            "next dev",
            "nodemon server.js",
            "lldb ./a.out",
            "gdb ./a.out",
            "docker compose up",
            "docker compose up --build",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
        }
        for c in [
            "docker compose up -d",
            "docker compose up --detach",
            "npm install",
            "npm run build",
        ] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn tail_follow_is_a_service_not_a_read() {
        for c in ["tail -f app.log", "tail -F app.log"] {
            assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
        }
        assert_eq!(blocked_tool("tail -n 5 app.log").as_deref(), Some("read"));
        assert_eq!(blocked_tool_with(&on(), &["read"], "tail -f app.log"), None);
    }

    #[test]
    fn watch_rule() {
        for c in [
            "cargo watch -x check -w src",
            "vitest --watch",
            "jest --watch",
            "tsc -w",
            "npm run build --watch",
            "npm run build\n--watch",
            "pytest --watch",
        ] {
            assert_eq!(blocked_tool(c).as_deref(), Some("bg"), "{c}");
        }
        for c in [
            "cargo watch -x test",
            "tsc --noEmit",
            "echo --watch",
            "pytest -q",
        ] {
            assert_eq!(blocked_tool(c), None, "{c}");
        }
    }

    #[test]
    fn js_dot_excludes_carriage_return() {
        // JS `.` does not match `\r`, so `(?:.|\n)*` cannot span it.
        assert_eq!(blocked_tool("npm run build\r--watch"), None);
    }

    #[test]
    fn pipe_stdin_segments_are_exempt() {
        assert_eq!(blocked_tool("printf x | cat"), None);
        assert_eq!(blocked_tool("echo data | grep foo"), None);
        assert_eq!(blocked_tool("ls | grep foo"), None);
        // The first stage of a pipeline is still a standalone command.
        assert_eq!(blocked_tool("cat file | wc -l").as_deref(), Some("read"));
    }

    #[test]
    fn leading_assignments_are_stripped_for_matching() {
        assert_eq!(
            blocked_tool("FOO=1 BAR=\"x y\" grep foo file").as_deref(),
            Some("grep")
        );
        assert_eq!(blocked_tool("A=1 cat x | wc -l").as_deref(), Some("read"));
        assert_eq!(blocked_tool("FOO=1 ls"), None);
        assert_eq!(blocked_tool("FOO=1"), None);
    }

    #[test]
    fn tool_availability_gates_rules() {
        assert_eq!(check(&on(), &["grep"], "cat x"), None);
        assert!(check(&on(), &["read"], "cat x").is_some());
        assert_eq!(check(&on(), &["bash"], "npm run dev"), None);
        assert_eq!(check(&hint_mode(), &["bash"], "grep -rn foo src"), None);
    }

    #[test]
    fn custom_patterns_override_defaults() {
        let config = BashInterceptorConfig {
            enabled: true,
            patterns: Some(vec![rule(r"^\s*rm\s+", "bash", "no rm")]),
            ..on()
        };
        assert_eq!(
            blocked(&config, "rm -rf x").as_deref(),
            Some("Blocked: no rm\n\nOriginal command: rm -rf x")
        );
        assert_eq!(check(&config, TOOLS, "cat x"), None);
        let hinting = BashInterceptorConfig {
            mode: InterceptMode::Hint,
            ..config
        };
        assert_eq!(
            check(&hinting, TOOLS, "rm -rf x"),
            Some(Verdict::Hint("[hint] no rm".into()))
        );

        let empty = BashInterceptorConfig {
            patterns: Some(Vec::new()),
            ..on()
        };
        assert_eq!(check(&empty, TOOLS, "cat x"), None);
    }

    #[test]
    fn invalid_regex_and_flags_are_skipped() {
        let config = BashInterceptorConfig {
            patterns: Some(vec![
                rule("(", "bash", "bad regex"),
                Rule {
                    flags: Some("q".into()),
                    ..rule("^x", "bash", "bad flag")
                },
                Rule {
                    flags: Some("gg".into()),
                    ..rule("^x", "bash", "dup flag")
                },
                Rule {
                    flags: Some("i".into()),
                    ..rule("^cat", "read", "ci")
                },
            ]),
            ..on()
        };
        assert_eq!(
            blocked(&config, "CAT x").as_deref(),
            Some("Blocked: ci\n\nOriginal command: CAT x")
        );
    }

    #[test]
    fn sticky_and_multiline_flags() {
        let sticky = BashInterceptorConfig {
            patterns: Some(vec![Rule {
                flags: Some("y".into()),
                ..rule("cat", "read", "sticky")
            }]),
            ..on()
        };
        assert!(check(&sticky, TOOLS, "cat x").is_some());
        assert!(check(&sticky, TOOLS, "x cat").is_none());

        let multi = BashInterceptorConfig {
            patterns: Some(vec![Rule {
                flags: Some("m".into()),
                ..rule("^cat", "read", "multi")
            }]),
            ..on()
        };
        // Candidates are split on `\n`, so this also works via the whole command.
        assert!(check(&multi, TOOLS, "ls\ncat x").is_some());
    }

    #[test]
    fn config_and_rule_serde_are_camel_case_and_strict() {
        let rule: Rule = serde_json::from_str(
            r#"{"pattern":"^x","tool":"read","message":"m","allowSubcommands":["a"]}"#,
        )
        .unwrap();
        assert_eq!(rule.allow_subcommands, Some(vec!["a".to_owned()]));
        assert!(
            serde_json::from_str::<Rule>(
                r#"{"pattern":"^x","tool":"read","message":"m","bogus":1}"#
            )
            .is_err()
        );

        let config: BashInterceptorConfig = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert!(config.enabled);
        assert_eq!(config.patterns, None);
        assert!(
            serde_json::from_str::<BashInterceptorConfig>(r#"{"enabled":true,"extra":1}"#).is_err()
        );
    }

    #[test]
    fn hint_mode_hints_a_lone_duplicate() {
        for (c, tool) in [
            ("grep -rn \"fn check\" src", "grep"),
            ("rg -n foo src", "grep"),
            ("cat Cargo.toml", "read"),
            ("sed -n 120,185p src/tools/execution.rs", "read"),
            ("find . -name '*.rs' -type f", "glob"),
            ("sed -i '' 's/a/b/' src/lib.rs", "edit"),
            ("npm run dev", "bg"),
            ("node server.js &", "bg"),
            // A leading `cd DIR` and output trimming do not change what the command is.
            ("cd crates/core && grep -rn foo src", "grep"),
            ("grep -n \"QListWidget\" src/ui/settings.py | head", "grep"),
            ("cat log.txt | tail -n 40", "read"),
            ("RUST_LOG=debug cat x", "read"),
        ] {
            assert_eq!(hinted_tool(c).as_deref(), Some(tool), "{c}");
        }
    }

    #[test]
    fn hint_mode_leaves_batched_and_scripted_commands_alone() {
        // Commands taken from session transcripts: batching several reads into one call saves
        // model round trips, and piping into another program is shell work a tool cannot do.
        for c in [
            "sed -n 30,60p src/ui/overlay/settings.rs; sed -n 285,345p src/ui/overlay/settings.rs",
            "grep -n \"set_title_fields\" src/ui/main_page.py; grep -n \"def title_fields\" src/ui/settings.py",
            "grep '^version' pyproject.toml && grep '__version__' src/simple_stipple/__init__.py",
            "grep -ohE \"page\\.[a-z_]+\" tests/test_shipping_ui.py | sort | uniq -c | sort -rn",
            "cat file | wc -l",
            "find . -name '*.rs' | xargs wc -l",
            "sed -i '' 's/a/b/' .tmp/audit.py && .venv/bin/python .tmp/audit.py",
            "cat > .tmp/set.py <<'EOF'\nprint(1)\nEOF",
            "tail -f app.log | grep ERROR",
            "grep -q foo f && echo yes",
            "cd src && ls",
            "ls -la",
            "cargo test --lib bash_interceptor",
            "git grep -n foo",
        ] {
            assert_eq!(check(&hint_mode(), TOOLS, c), None, "{c}");
        }
        // Block mode still inspects every standalone segment.
        assert!(matches!(
            check(&on(), TOOLS, "cargo build; grep -rn foo src"),
            Some(Verdict::Block(_))
        ));
    }

    #[test]
    fn hint_is_one_line() {
        for c in [
            "grep -rn foo src",
            "cat x",
            "npm run dev",
            "sed -i s/a/b/ f",
        ] {
            let Some(Verdict::Hint(line)) = check(&hint_mode(), TOOLS, c) else {
                panic!("expected a hint for {c}");
            };
            assert!(
                line.starts_with("[hint] ") && !line.contains('\n'),
                "{line}"
            );
        }
    }

    #[test]
    fn ci_poll_hint_fires_on_unfinished_status_or_sleep_loop() {
        let config = hint_mode();
        let view = "gh run view 38768263506 -R KiidxAtlas/dial --json status,conclusion,jobs --jq '\"run: \\(.status) \\(.conclusion)\"'";
        let running = "run: in_progress \nBuild and package Windows release: in_progress \n";
        let hint = ci_poll_hint(&config, TOOLS, view, running).expect("in_progress view");
        assert!(
            hint.contains("gh run watch") && hint.contains("bg"),
            "{hint}"
        );
        assert!(!hint.contains('\n'));
        assert!(
            ci_poll_hint(&config, &["github"], view, running)
                .is_some_and(|hint| hint.contains("run_watch"))
        );
        assert!(
            ci_poll_hint(&config, &[], view, running)
                .is_some_and(|hint| hint.contains("gh run watch") && !hint.contains("`bg`"))
        );
        for (command, output) in [
            (
                "gh run list -L 3",
                "in_progress\t\tfix ci\tci\tmain\tpush\t1\t39s\n",
            ),
            ("gh pr checks 12", "build\tpending\t0\thttps://x\n"),
            (
                "gh run list -L 3 --json status",
                "[{\"status\":\"queued\"}]",
            ),
            (
                "for i in $(seq 1 19); do s=$(gh run view 1 --json status --jq .status); [ \"$s\" = completed ] && break; sleep 15; done",
                "command is running as background job bg20 (after 15s)",
            ),
        ] {
            assert!(
                ci_poll_hint(&config, TOOLS, command, output).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn ci_poll_hint_ignores_finished_runs_logs_and_watches() {
        let config = hint_mode();
        for (command, output) in [
            ("gh run view 1 --json status", "run: completed success\n"),
            ("gh run view 1 --log-failed", "step in_progress marker\n"),
            (
                "gh run watch 1 --exit-status > /dev/null; gh run view 1",
                "command is running as background job bg3",
            ),
            ("gh pr view 12", "state: OPEN pending review\n"),
            (
                "sleep 10; id=$(gh run list --limit 1 --json databaseId -q '.[0].databaseId'); gh run watch $id --exit-status",
                "command is running as background job bg5",
            ),
            ("cargo test", "test in_progress ... ok\n"),
        ] {
            assert_eq!(
                ci_poll_hint(&config, TOOLS, command, output),
                None,
                "{command}"
            );
        }
    }

    #[test]
    fn flat_segments_split_pipes_and_lists() {
        assert_eq!(
            segments("a; b | c && d"),
            vec![
                ("a".to_owned(), false),
                ("b".to_owned(), false),
                ("c".to_owned(), true),
                ("d".to_owned(), false),
            ]
        );
    }

    #[test]
    fn flat_segments_respect_quotes() {
        assert_eq!(
            segments("echo \"a;b\" && c"),
            vec![("echo \"a;b\"".to_owned(), false), ("c".to_owned(), false)]
        );
    }

    #[test]
    fn flat_segments_keep_fd_duplication_and_stderr_pipe() {
        assert_eq!(
            segments("a 2>&1 | cat"),
            vec![("a 2>&1".to_owned(), false), ("cat".to_owned(), true)]
        );
        assert_eq!(
            segments("a |& b"),
            vec![("a".to_owned(), false), ("b".to_owned(), true)]
        );
    }

    #[test]
    fn flat_segments_comment_is_a_boundary() {
        assert_eq!(
            segments("a # note\nb"),
            vec![("a".to_owned(), false), ("b".to_owned(), false)]
        );
    }

    #[test]
    fn flat_segments_decline_unscannable_syntax() {
        assert!(segments("cat <<EOF").is_empty());
        assert!(segments("echo $(ls)").is_empty());
        assert!(segments("echo `ls`").is_empty());
        assert!(segments("(cd x; ls)").is_empty());
        assert!(segments("echo 'x").is_empty());
        assert!(segments("echo \\").is_empty());
    }

    #[test]
    fn leading_assignment_stripping() {
        assert_eq!(
            without_leading_environment_assignments("FOO=\"a b\" cmd arg").as_deref(),
            Some("cmd arg")
        );
        assert_eq!(without_leading_environment_assignments("FOO=\"a"), None);
        assert_eq!(without_leading_environment_assignments("FOO=1"), None);
        assert_eq!(without_leading_environment_assignments("ls -la"), None);
    }
}

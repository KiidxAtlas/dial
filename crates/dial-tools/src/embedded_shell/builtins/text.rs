//! Text-processing commands for the built-in shell: `cut`, `env`, `grep`, `sed`, `sort`, `tr`
//! and `uniq`. They run in-process and never spawn a program. `xargs` and `find` run other
//! programs and are not implemented in this module.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::str::Chars;

use regex::{Regex, RegexBuilder};

use super::Io;

/// Parsed command-line options: switch letters, value options in order, and operands.
#[derive(Default)]
struct Opts {
    switches: String,
    given: Vec<(char, String)>,
    operands: Vec<String>,
}

impl Opts {
    fn has(&self, switch: char) -> bool {
        self.switches.contains(switch)
    }

    /// All values given for `flag`, in order.
    fn values(&self, flag: char) -> Vec<&str> {
        self.given
            .iter()
            .filter(|(c, _)| *c == flag)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// Splits `args` into switches, value options and operands. `switches` and `with_value` list the
/// accepted option letters. With `stop_at_operand`, the first operand ends option parsing; `xargs`
/// needs this because its command arguments may themselves start with `-`.
fn parse_opts(
    args: &[String],
    switches: &str,
    with_value: &str,
    stop_at_operand: bool,
) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg.as_str() == "--" {
            opts.operands.extend(rest.by_ref().cloned());
            break;
        }
        if !arg.starts_with('-') || arg.as_str() == "-" {
            opts.operands.push(arg.clone());
            if stop_at_operand {
                opts.operands.extend(rest.by_ref().cloned());
                break;
            }
            continue;
        }
        let mut chars = arg[1..].chars();
        while let Some(flag) = chars.next() {
            if with_value.contains(flag) {
                let attached: String = chars.by_ref().collect();
                let value = if attached.is_empty() {
                    rest.next()
                        .cloned()
                        .ok_or_else(|| format!("option requires an argument -- '{flag}'"))?
                } else {
                    attached
                };
                opts.given.push((flag, value));
                break;
            }
            if !switches.contains(flag) {
                return Err(format!("invalid option -- '{flag}'"));
            }
            opts.switches.push(flag);
        }
    }
    Ok(opts)
}

/// Operand list where no operands means standard input.
fn inputs(operands: &[String]) -> Vec<&str> {
    if operands.is_empty() {
        vec!["-"]
    } else {
        operands.iter().map(String::as_str).collect()
    }
}

/// Strips Rust's `(os error N)` suffix so messages read as GNU coreutils prints them.
fn os_message(error: &std::io::Error) -> String {
    let text = error.to_string();
    match text.find(" (os error") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// Reads a whole input: `-` is standard input, anything else a file relative to the shell cwd.
fn read_input(io: &mut Io<'_>, name: &str) -> Result<Vec<u8>, String> {
    if name == "-" {
        let mut buf = Vec::new();
        io.stdin
            .read_to_end(&mut buf)
            .map_err(|e| format!("standard input: {}", os_message(&e)))?;
        return Ok(buf);
    }
    std::fs::read(io.path(name)).map_err(|e| format!("{name}: {}", os_message(&e)))
}

fn read_text(io: &mut Io<'_>, name: &str) -> Result<String, String> {
    read_input(io, name).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Splits text into lines without their terminating `\n`. Empty text has no lines; a final
/// unterminated line still counts.
fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect()
}

// ---------------------------------------------------------------------------------------------
// cut
// ---------------------------------------------------------------------------------------------

pub(super) fn cut(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "", "df", false) {
        Ok(opts) => opts,
        Err(message) => return io.decline(message),
    };
    let Some(list) = opts.values('f').last().copied() else {
        return io.usage();
    };
    let delim = match opts.values('d').last().copied() {
        None => '\t',
        Some(text) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c,
                _ => return io.fail("the delimiter must be a single character"),
            }
        }
    };
    let ranges = match parse_field_list(list) {
        Ok(ranges) => ranges,
        Err(message) => return io.fail(message),
    };
    let delim_text = delim.to_string();
    let mut status = 0;
    for input in inputs(&opts.operands) {
        let text = match read_text(io, input) {
            Ok(text) => text,
            Err(message) => {
                status = io.fail(message);
                continue;
            }
        };
        for line in split_lines(&text) {
            if !line.contains(delim) {
                let _ = writeln!(io.stdout, "{line}");
                continue;
            }
            let picked: Vec<&str> = line
                .split(delim)
                .enumerate()
                .filter(|(index, _)| in_ranges(&ranges, index + 1))
                .map(|(_, field)| field)
                .collect();
            let _ = writeln!(io.stdout, "{}", picked.join(delim_text.as_str()));
        }
    }
    status
}

/// Parses a `-f` list such as `1,3-4,6-` or `-2` into inclusive `(first, last)` ranges.
fn parse_field_list(spec: &str) -> Result<Vec<(usize, usize)>, String> {
    spec.split(',')
        .map(|item: &str| -> Result<(usize, usize), String> {
            match item.split_once('-') {
                None => field_number(item).map(|n| (n, n)),
                Some(("", "")) => Err(format!("invalid field value '{item}'")),
                Some(("", last)) => field_number(last).map(|n| (1, n)),
                Some((first, "")) => field_number(first).map(|n| (n, usize::MAX)),
                Some((first, last)) => {
                    let (first, last) = (field_number(first)?, field_number(last)?);
                    if last < first {
                        Err("invalid decreasing range".to_owned())
                    } else {
                        Ok((first, last))
                    }
                }
            }
        })
        .collect()
}

fn field_number(text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(0) => Err("fields are numbered from 1".to_owned()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("invalid field value '{text}'")),
    }
}

fn in_ranges(ranges: &[(usize, usize)], n: usize) -> bool {
    ranges
        .iter()
        .any(|&(first, last)| (first..=last).contains(&n))
}

// ---------------------------------------------------------------------------------------------
// env
// ---------------------------------------------------------------------------------------------

pub(super) fn env(io: &mut Io<'_>, args: &[String]) -> i32 {
    if !args.is_empty() {
        return io.usage();
    }
    let vars = io.env;
    let mut sorted: Vec<&(String, String)> = vars.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in sorted {
        let _ = writeln!(io.stdout, "{name}={value}");
    }
    0
}

// ---------------------------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------------------------

pub(super) fn grep(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "ivnclrEFqhHoxws", "e", false) {
        Ok(opts) => opts,
        Err(message) => return io.decline(message),
    };
    // `-e` supplies every pattern, so all operands are files; otherwise the first operand is the
    // pattern.
    let explicit = opts.values('e');
    let (patterns, files): (Vec<&str>, &[String]) = if explicit.is_empty() {
        match opts.operands.split_first() {
            Some((pattern, files)) => (vec![pattern.as_str()], files),
            None => return io.usage(),
        }
    } else {
        (explicit, opts.operands.as_slice())
    };
    let whole = if opts.has('x') {
        Whole::Line
    } else if opts.has('w') {
        Whole::Word
    } else {
        Whole::Any
    };
    let re = match grep_regex(&patterns, opts.has('F'), opts.has('i'), whole) {
        Ok(re) => re,
        Err(error) => {
            let _ = writeln!(io.stderr, "{}: {error}", io.name);
            return 2;
        }
    };
    let invert = opts.has('v');
    let line_numbers = opts.has('n');
    let count_only = opts.has('c');
    let list_only = opts.has('l');
    let quiet = opts.has('q');
    let only_matching = opts.has('o');
    let recursive = opts.has('r');
    // -s drops messages about missing or unreadable files; the exit status still reports them.
    let silent = opts.has('s');

    let default_target = if recursive { "." } else { "-" };
    let targets: Vec<&str> = if files.is_empty() {
        vec![default_target]
    } else {
        files.iter().map(String::as_str).collect()
    };
    // Several inputs (or -r) prefix each line with its name; -H forces the prefix, -h drops it.
    let with_name = opts.has('H') || (!opts.has('h') && (targets.len() > 1 || recursive));

    let mut had_error = false;
    let mut inputs: Vec<(String, Option<PathBuf>)> = Vec::new();
    for target in targets {
        if target == "-" {
            inputs.push(("(standard input)".to_owned(), None));
            continue;
        }
        let mut errors = Vec::new();
        expand(
            &io.path(target),
            target.to_owned(),
            recursive,
            &mut inputs,
            &mut errors,
        );
        for error in errors {
            had_error = true;
            if !silent {
                let _ = writeln!(io.stderr, "{}: {error}", io.name);
            }
        }
    }

    let mut total = 0;
    for (label, path) in &inputs {
        // Lines are read one at a time, so -q returns at the first selected line without reading the
        // rest of standard input.
        let mut reader: Box<dyn BufRead + '_> = match path {
            Some(path) => match std::fs::File::open(path) {
                Ok(file) => Box::new(BufReader::new(file)),
                Err(error) => {
                    had_error = true;
                    if !silent {
                        let _ = writeln!(io.stderr, "{}: {label}: {}", io.name, os_message(&error));
                    }
                    continue;
                }
            },
            None => Box::new(BufReader::new(&mut *io.stdin)),
        };
        let mut matched = 0;
        let mut buf = Vec::new();
        let mut number = 0;
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => {
                    had_error = true;
                    let _ = writeln!(io.stderr, "{}: {label}: {}", io.name, os_message(&error));
                    break;
                }
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            number += 1;
            let line = String::from_utf8_lossy(&buf);
            if grep_matches(&re, whole, &line).is_empty() != invert {
                continue;
            }
            matched += 1;
            if quiet {
                // GNU grep -q exits as soon as a line is selected, even after an unreadable file.
                return 0;
            }
            if list_only || count_only {
                continue;
            }
            let line_number = line_numbers.then_some(number);
            if !only_matching {
                let prefix = line_prefix(label, with_name, line_number);
                let _ = writeln!(io.stdout, "{prefix}{line}");
            } else if !invert {
                // -o prints each non-empty match on its own line; a -v selection has none.
                for found in grep_matches(&re, whole, &line)
                    .into_iter()
                    .filter(|found| !found.is_empty())
                {
                    let prefix = line_prefix(label, with_name, line_number);
                    let _ = writeln!(io.stdout, "{prefix}{}", found.as_str());
                }
            }
        }
        if list_only {
            if matched > 0 {
                let _ = writeln!(io.stdout, "{label}");
            }
        } else if count_only {
            if with_name {
                let _ = writeln!(io.stdout, "{label}:{matched}");
            } else {
                let _ = writeln!(io.stdout, "{matched}");
            }
        }
        total += matched;
    }

    // Like GNU grep, an unreadable file makes the exit status 2 even when lines matched.
    if had_error {
        2
    } else if total > 0 {
        0
    } else {
        1
    }
}

/// What a grep match must span: anywhere in the line, a whole word (`-w`), or the whole line (`-x`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Whole {
    Any,
    Word,
    Line,
}

/// Compiles the patterns into one regex that matches when any of them does. Each pattern is
/// compiled on its own first, so that one cannot close a group opened by another. With `-w` a
/// match must not touch a word character (letter, digit or `_`) on either side; with `-x` it must
/// be the whole line.
fn grep_regex(
    patterns: &[&str],
    fixed: bool,
    ignore_case: bool,
    whole: Whole,
) -> Result<Regex, regex::Error> {
    let mut alternatives = Vec::new();
    for pattern in patterns.iter().flat_map(|pattern| pattern.split('\n')) {
        let source = if fixed {
            regex::escape(pattern)
        } else {
            pattern.to_owned()
        };
        RegexBuilder::new(&source)
            .case_insensitive(ignore_case)
            .build()?;
        alternatives.push(format!("(?:{source})"));
    }
    let any = alternatives.join("|");
    let source = match whole {
        // Word boundaries are checked per match by `grep_matches`: a boundary consumed by the
        // regex would show up in `-o` output, and the regex crate has no lookaround.
        Whole::Any | Whole::Word => any,
        Whole::Line => format!("^(?:{any})$"),
    };
    RegexBuilder::new(&source)
        .case_insensitive(ignore_case)
        .build()
}

/// The `name:` and `number:` prefix of a grep output line.
fn line_prefix(label: &str, with_name: bool, number: Option<usize>) -> String {
    let name = if with_name {
        format!("{label}:")
    } else {
        String::new()
    };
    let number = number.map_or_else(String::new, |n| format!("{n}:"));
    format!("{name}{number}")
}

/// The non-overlapping matches grep selects in `line`. Under `-w` a candidate is kept only when no
/// word character (letter, digit or `_`) touches it on either side; otherwise the search resumes
/// one character after the candidate's start, so a later word-bounded match is still found.
fn grep_matches<'h>(re: &Regex, whole: Whole, line: &'h str) -> Vec<regex::Match<'h>> {
    if whole != Whole::Word {
        return re.find_iter(line).collect();
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let next_len = |at: usize| line[at..].chars().next().map_or(1, char::len_utf8);
    let mut found = Vec::new();
    let mut at = 0;
    while at <= line.len() {
        let Some(candidate) = re.find_at(line, at) else {
            break;
        };
        let touches_before = line[..candidate.start()]
            .chars()
            .next_back()
            .is_some_and(is_word);
        let touches_after = line[candidate.end()..].chars().next().is_some_and(is_word);
        at = if touches_before || touches_after {
            candidate.start() + next_len(candidate.start())
        } else {
            found.push(candidate);
            if candidate.is_empty() {
                candidate.end() + next_len(candidate.end())
            } else {
                candidate.end()
            }
        };
    }
    found
}

/// Appends every file under `path` to `inputs` in sorted order. Directories are an error unless
/// `recursive`; symbolic links met during recursion are skipped, as GNU grep does without `-R`.
fn expand(
    path: &Path,
    label: String,
    recursive: bool,
    inputs: &mut Vec<(String, Option<PathBuf>)>,
    errors: &mut Vec<String>,
) {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) => {
            errors.push(format!("{label}: {}", os_message(&error)));
            return;
        }
    };
    if !meta.is_dir() {
        inputs.push((label, Some(path.to_path_buf())));
        return;
    }
    if !recursive {
        errors.push(format!("{label}: Is a directory"));
        return;
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("{label}: {}", os_message(&error)));
            return;
        }
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_symlink()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let child_label = if label.ends_with('/') {
            format!("{label}{name}")
        } else {
            format!("{label}/{name}")
        };
        expand(&path.join(&name), child_label, recursive, inputs, errors);
    }
}

// ---------------------------------------------------------------------------------------------
// sed (`s`, `p` and `d`, with line, `$` and regex addresses)
// ---------------------------------------------------------------------------------------------

/// One piece of a replacement: literal text or a capture group (`0` is the whole match).
enum Part {
    Text(String),
    Group(usize),
}

/// `s/RE/REPL/FLAGS`. Replacement starts at match number `occurrence` (1 unless the `N` flag says
/// otherwise); `global` (`g`) replaces every match from there on.
struct Subst {
    regex: Regex,
    parts: Vec<Part>,
    occurrence: usize,
    global: bool,
    print: bool,
}

enum Action {
    Subst(Subst),
    Print,
    Delete,
}

/// A line number, the last line, or every line matching a regex.
enum Address {
    Line(usize),
    Last,
    Match(Regex),
}

impl Address {
    /// Whether this address selects the line.
    fn matches(&self, number: usize, last: bool, pattern: &str) -> bool {
        match self {
            Address::Line(n) => *n == number,
            Address::Last => last,
            Address::Match(re) => re.is_match(pattern),
        }
    }
}

/// Which lines a command applies to. A range runs from its first address to its second, inclusive.
enum Selector {
    All,
    Single(Address),
    Range(Address, Address),
}

struct Command {
    selector: Selector,
    action: Action,
}

/// Why a script was refused. `Unsupported` covers every construct outside the supported subset.
enum ScriptError {
    Unsupported,
    Regex(regex::Error),
    Reference(usize),
}

pub(super) fn sed(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "n", "e", false) {
        Ok(opts) => opts,
        Err(message) => return io.decline(message),
    };
    // `-e` scripts are joined with newlines; without `-e` the first operand is the script.
    let scripts = opts.values('e');
    let (script, files): (String, &[String]) = if scripts.is_empty() {
        match opts.operands.split_first() {
            Some((script, rest)) => (script.clone(), rest),
            None => return io.usage(),
        }
    } else {
        (scripts.join("\n"), opts.operands.as_slice())
    };
    let commands = match parse_script(&script) {
        Ok(commands) => commands,
        Err(ScriptError::Unsupported) => return io.decline("unsupported sed script"),
        Err(ScriptError::Regex(error)) => {
            let _ = writeln!(io.stderr, "{}: {error}", io.name);
            return 2;
        }
        Err(ScriptError::Reference(group)) => {
            return io.fail(format!("invalid reference \\{group} on `s' command's RHS"));
        }
    };

    // Every input is read before any output, because `$` is the last line of the combined input.
    let mut lines: Vec<String> = Vec::new();
    let mut status = 0;
    for input in inputs(files) {
        match read_text(io, input) {
            Ok(text) => lines.extend(split_lines(&text).into_iter().map(str::to_owned)),
            Err(message) => status = io.fail(message),
        }
    }

    let quiet = opts.has('n');
    let total = lines.len();
    // Per command: whether a range is open after the previous line.
    let mut open = vec![false; commands.len()];
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        let last = number == total;
        let mut pattern = line.clone();
        let mut deleted = false;
        for (command, range_open) in commands.iter().zip(open.iter_mut()) {
            if !selects(&command.selector, range_open, number, last, &pattern) {
                continue;
            }
            match &command.action {
                Action::Print => {
                    let _ = writeln!(io.stdout, "{pattern}");
                }
                Action::Delete => {
                    deleted = true;
                    break;
                }
                Action::Subst(subst) => {
                    if let Some(replaced) = substitute(subst, &pattern) {
                        pattern = replaced;
                        if subst.print {
                            let _ = writeln!(io.stdout, "{pattern}");
                        }
                    }
                }
            }
        }
        if !deleted && !quiet {
            let _ = writeln!(io.stdout, "{pattern}");
        }
    }
    status
}

/// Whether a command's selector picks the current line. `range_open` records a range that began
/// on an earlier line and has not closed yet; it is updated here.
fn selects(
    selector: &Selector,
    range_open: &mut bool,
    number: usize,
    last: bool,
    pattern: &str,
) -> bool {
    match selector {
        Selector::All => true,
        Selector::Single(address) => address.matches(number, last, pattern),
        Selector::Range(start, end) => {
            if *range_open {
                return match end {
                    // A line-number end closes the range on that line; a later line is outside it.
                    Address::Line(n) => {
                        *range_open = number < *n;
                        number <= *n
                    }
                    _ => {
                        *range_open = !end.matches(number, last, pattern);
                        true
                    }
                };
            }
            if !start.matches(number, last, pattern) {
                return false;
            }
            // A line-number end at or before the start selects only this line, and a regex end
            // is first tested on the following line.
            *range_open = match end {
                Address::Line(n) => *n > number,
                Address::Last => !last,
                Address::Match(_) => true,
            };
            true
        }
    }
}

/// Parses a script of commands separated by `;` or newlines. Each command is `s/RE/REPL/FLAGS`,
/// `p` or `d`, optionally preceded by an address: `N`, `$` or `/RE/`, or a pair `A,B` of them.
fn parse_script(script: &str) -> Result<Vec<Command>, ScriptError> {
    let mut chars = script.chars();
    let mut commands = Vec::new();
    loop {
        take_while(&mut chars, |c| c == ';' || c == '\n' || c.is_whitespace());
        if peek(&chars).is_none() {
            return Ok(commands);
        }
        let selector = parse_selector(&mut chars)?;
        take_while(&mut chars, is_blank);
        let action = match chars.next() {
            Some('p') => Action::Print,
            Some('d') => Action::Delete,
            Some('s') => parse_subst(&mut chars)?,
            _ => return Err(ScriptError::Unsupported),
        };
        take_while(&mut chars, is_blank);
        if !matches!(peek(&chars), None | Some(';' | '\n')) {
            return Err(ScriptError::Unsupported);
        }
        commands.push(Command { selector, action });
    }
}

fn parse_selector(chars: &mut Chars<'_>) -> Result<Selector, ScriptError> {
    let Some(start) = parse_address(chars)? else {
        return Ok(Selector::All);
    };
    if peek(chars) != Some(',') {
        return Ok(Selector::Single(start));
    }
    chars.next();
    let end = parse_address(chars)?.ok_or(ScriptError::Unsupported)?;
    Ok(Selector::Range(start, end))
}

/// Parses one address if the next character starts one. Line numbers start at 1.
fn parse_address(chars: &mut Chars<'_>) -> Result<Option<Address>, ScriptError> {
    match peek(chars) {
        Some('$') => {
            chars.next();
            Ok(Some(Address::Last))
        }
        Some('/') => {
            chars.next();
            let source = scan_part(chars).ok_or(ScriptError::Unsupported)?;
            // An empty regex means "reuse the last regex", which this subset does not track.
            if source.is_empty() {
                return Err(ScriptError::Unsupported);
            }
            let re = RegexBuilder::new(&source)
                .build()
                .map_err(ScriptError::Regex)?;
            Ok(Some(Address::Match(re)))
        }
        Some('0'..='9') => match take_while(chars, |c| c.is_ascii_digit()).parse::<usize>() {
            Ok(n) if n > 0 => Ok(Some(Address::Line(n))),
            _ => Err(ScriptError::Unsupported),
        },
        _ => Ok(None),
    }
}

/// Parses the rest of an `s` command after its letter: `/RE/REPL/` and the flags `g`, `p`, `i` or
/// `I` (ignore case) and `N` (first match to replace). `\/` is a literal slash in both parts.
fn parse_subst(chars: &mut Chars<'_>) -> Result<Action, ScriptError> {
    if chars.next() != Some('/') {
        return Err(ScriptError::Unsupported);
    }
    let pattern = scan_part(chars).ok_or(ScriptError::Unsupported)?;
    // An empty pattern would reuse the last regex, which this subset does not track.
    if pattern.is_empty() {
        return Err(ScriptError::Unsupported);
    }
    let replacement = scan_part(chars).ok_or(ScriptError::Unsupported)?;
    let (mut global, mut print, mut ignore_case) = (false, false, false);
    let mut occurrence: usize = 1;
    loop {
        match peek(chars) {
            Some('g') => global = true,
            Some('p') => print = true,
            Some('i' | 'I') => ignore_case = true,
            Some('1'..='9') => {
                occurrence = take_while(chars, |c| c.is_ascii_digit())
                    .parse::<usize>()
                    .map_err(|_| ScriptError::Unsupported)?;
                continue;
            }
            None | Some(' ' | '\t' | ';' | '\n') => break,
            Some(_) => return Err(ScriptError::Unsupported),
        }
        chars.next();
    }
    let regex = RegexBuilder::new(&pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(ScriptError::Regex)?;
    let parts = parse_replacement(&replacement);
    let max_ref = parts
        .iter()
        .filter_map(|part| match part {
            Part::Group(index) => Some(*index),
            Part::Text(_) => None,
        })
        .max()
        .unwrap_or(0);
    if max_ref >= regex.captures_len() {
        return Err(ScriptError::Reference(max_ref));
    }
    Ok(Action::Subst(Subst {
        regex,
        parts,
        occurrence,
        global,
        print,
    }))
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// The next character without consuming it.
fn peek(chars: &Chars<'_>) -> Option<char> {
    chars.clone().next()
}

/// Consumes characters while `keep` holds and returns them.
fn take_while(chars: &mut Chars<'_>, keep: impl Fn(char) -> bool) -> String {
    let mut taken = String::new();
    while let Some(c) = peek(chars).filter(|c| keep(*c)) {
        taken.push(c);
        chars.next();
    }
    taken
}

/// Applies the substitution to one line. `None` when it replaces nothing.
fn substitute(subst: &Subst, line: &str) -> Option<String> {
    let mut out = String::new();
    let mut copied = 0;
    let mut replaced = false;
    let mut seen = 0;
    for caps in subst.regex.captures_iter(line) {
        let Some(whole) = caps.get(0) else {
            continue;
        };
        seen += 1;
        if seen < subst.occurrence {
            continue;
        }
        if seen > subst.occurrence && !subst.global {
            break;
        }
        out.push_str(&line[copied..whole.start()]);
        for part in &subst.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Group(index) => {
                    if let Some(group) = caps.get(*index) {
                        out.push_str(group.as_str());
                    }
                }
            }
        }
        copied = whole.end();
        replaced = true;
    }
    if !replaced {
        return None;
    }
    out.push_str(&line[copied..]);
    Some(out)
}

/// Reads up to the next unescaped `/`, which is consumed. `None` if the delimiter is missing.
fn scan_part(chars: &mut std::str::Chars<'_>) -> Option<String> {
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '/' => return Some(out),
            '\\' => match chars.next()? {
                '/' => out.push('/'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            _ => out.push(c),
        }
    }
    None
}

/// Splits a replacement into text and groups: `&` and `\0`..`\9` refer to groups, `\n` is a
/// newline, and any other `\c` is the literal character `c`.
fn parse_replacement(raw: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '&' => {
                if !text.is_empty() {
                    parts.push(Part::Text(std::mem::take(&mut text)));
                }
                parts.push(Part::Group(0));
            }
            '\\' => match chars.next() {
                Some(digit) if digit.is_ascii_digit() => {
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(Part::Group(usize::from(digit as u8 - b'0')));
                }
                Some('n') => text.push('\n'),
                Some(other) => text.push(other),
                None => text.push('\\'),
            },
            _ => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    parts
}

// ---------------------------------------------------------------------------------------------
// sort
// ---------------------------------------------------------------------------------------------

pub(super) fn sort(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "rnu", "", false) {
        Ok(opts) => opts,
        Err(message) => return io.decline(message),
    };
    let numeric = opts.has('n');
    let mut lines: Vec<(f64, String)> = Vec::new();
    for input in inputs(&opts.operands) {
        let text = match read_text(io, input) {
            Ok(text) => text,
            Err(message) => return io.fail(message),
        };
        lines.extend(split_lines(&text).into_iter().map(|line| {
            let key = if numeric { numeric_key(line) } else { 0.0 };
            (key, line.to_owned())
        }));
    }
    lines.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    if opts.has('r') {
        lines.reverse();
    }
    if opts.has('u') {
        // Equal lines (or, with -n, equal numeric keys) are adjacent after sorting.
        lines.dedup_by(|later, kept| {
            if numeric {
                later.0 == kept.0
            } else {
                later.1 == kept.1
            }
        });
    }
    for (_, line) in &lines {
        let _ = writeln!(io.stdout, "{line}");
    }
    0
}

/// Leading numeric value of a line for `sort -n`: optional blanks, an optional sign, digits and an
/// optional fraction. Lines without a number compare as 0.
fn numeric_key(line: &str) -> f64 {
    let bytes = line.trim_start().as_bytes();
    let mut end = 0;
    if matches!(bytes.first().copied(), Some(b'+') | Some(b'-')) {
        end = 1;
    }
    let int_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let mut digits = end - int_start;
    if end < bytes.len() && bytes[end] == b'.' {
        let mut frac_end = end + 1;
        while frac_end < bytes.len() && bytes[frac_end].is_ascii_digit() {
            frac_end += 1;
        }
        digits += frac_end - end - 1;
        end = frac_end;
    }
    if digits == 0 {
        return 0.0;
    }
    let value: f64 = std::str::from_utf8(&bytes[..end])
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(0.0);
    // Normalise -0.0 so that it sorts and deduplicates together with 0.
    if value == 0.0 { 0.0 } else { value }
}

// ---------------------------------------------------------------------------------------------
// tr
// ---------------------------------------------------------------------------------------------

pub(super) fn tr(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "d", "", false) {
        Ok(opts) => opts,
        Err(message) => return io.decline(message),
    };
    let delete = opts.has('d');
    let (set1, set2) = match opts.operands.as_slice() {
        [one] if delete => (one.as_str(), ""),
        [one, two] if !delete => (one.as_str(), two.as_str()),
        _ => return io.usage(),
    };
    let set1 = match expand_set(set1) {
        Ok(set) => set,
        Err(message) => return io.fail(message),
    };

    let mut input = Vec::new();
    if let Err(error) = io.stdin.read_to_end(&mut input) {
        return io.fail(format!("standard input: {}", os_message(&error)));
    }
    let text = String::from_utf8_lossy(&input);
    let out: String = if delete {
        text.chars().filter(|c| !set1.contains(c)).collect()
    } else {
        let set2 = match expand_set(set2) {
            Ok(set) => set,
            Err(message) => return io.fail(message),
        };
        if set2.is_empty() {
            return io.fail("when not deleting, SET2 must be non-empty");
        }
        // A short SET2 is padded by repeating its last character; a repeated SET1 character takes
        // its last mapping.
        let last = set2.len() - 1;
        let map: HashMap<char, char> = set1
            .iter()
            .enumerate()
            .map(|(index, c)| (*c, set2[index.min(last)]))
            .collect();
        text.chars()
            .map(|c| map.get(&c).copied().unwrap_or(c))
            .collect()
    };
    let _ = io.stdout.write_all(out.as_bytes());
    0
}

/// Expands a `tr` set: `a-z` ranges, and the escapes `\n`, `\t`, `\r`, `\\`; any other escaped
/// character stands for itself. A `-` that is first, last, or escaped is literal.
fn expand_set(spec: &str) -> Result<Vec<char>, String> {
    // (character, came from an escape)
    let mut tokens: Vec<(char, bool)> = Vec::new();
    let mut chars = spec.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            tokens.push((c, false));
            continue;
        }
        match chars.next() {
            Some('n') => tokens.push(('\n', true)),
            Some('t') => tokens.push(('\t', true)),
            Some('r') => tokens.push(('\r', true)),
            Some(other) => tokens.push((other, true)),
            None => tokens.push(('\\', true)),
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if i + 2 < tokens.len() && tokens[i + 1] == ('-', false) {
            let (first, last) = (tokens[i].0, tokens[i + 2].0);
            if first > last {
                return Err(format!(
                    "range-endpoints of '{first}-{last}' are in reverse collating sequence order"
                ));
            }
            out.extend(first..=last);
            i += 3;
        } else {
            out.push(tokens[i].0);
            i += 1;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// uniq
// ---------------------------------------------------------------------------------------------

pub(super) fn uniq(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "cdu", "", false) {
        Ok(opts) => opts,
        Err(message) => return io.decline(message),
    };
    if opts.operands.len() > 1 {
        return io.usage();
    }
    let input = opts.operands.first().map_or("-", String::as_str);
    let text = match read_text(io, input) {
        Ok(text) => text,
        Err(message) => return io.fail(message),
    };

    // Runs of adjacent equal lines, with their lengths.
    let mut runs: Vec<(&str, usize)> = Vec::new();
    for line in split_lines(&text) {
        match runs.last_mut() {
            Some(last) if last.0 == line => last.1 += 1,
            _ => runs.push((line, 1)),
        }
    }

    let duplicates_only = opts.has('d');
    let unique_only = opts.has('u');
    let counts = opts.has('c');
    for (line, count) in runs {
        if (duplicates_only && count < 2) || (unique_only && count > 1) {
            continue;
        }
        if counts {
            let _ = writeln!(io.stdout, "{count:>7} {line}");
        } else {
            let _ = writeln!(io.stdout, "{line}");
        }
    }
    0
}

#[cfg(test)]
mod tests;

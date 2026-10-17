//! Text-processing commands for the built-in shell: `cut`, `env`, `grep`, `sed` (substitution
//! only), `sort`, `tr`, `uniq` and `xargs`.
//!
//! `xargs` runs external programs only. It cannot run shell builtins or functions: a name that
//! exists only as a builtin is looked up on the filesystem, fails, and exits 127.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Reports a bad option or argument, then the usage line. Returns exit code 2.
fn usage_error(io: &mut Io<'_>, message: &str) -> i32 {
    let _ = writeln!(io.stderr, "{}: {message}", io.name);
    io.usage()
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
        Err(message) => return usage_error(io, &message),
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
    let opts = match parse_opts(args, "ivnclr", "", false) {
        Ok(opts) => opts,
        Err(message) => return usage_error(io, &message),
    };
    let Some((pattern, files)) = opts.operands.split_first() else {
        return io.usage();
    };
    let re = match RegexBuilder::new(pattern)
        .case_insensitive(opts.has('i'))
        .build()
    {
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
    let recursive = opts.has('r');

    let default_target = if recursive { "." } else { "-" };
    let targets: Vec<&str> = if files.is_empty() {
        vec![default_target]
    } else {
        files.iter().map(String::as_str).collect()
    };
    let with_name = targets.len() > 1 || recursive;

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
            let _ = writeln!(io.stderr, "{}: {error}", io.name);
        }
    }

    let mut total = 0;
    for (label, path) in &inputs {
        let bytes = match path {
            Some(path) => std::fs::read(path).map_err(|e| os_message(&e)),
            None => {
                let mut buf = Vec::new();
                io.stdin
                    .read_to_end(&mut buf)
                    .map(|_| buf)
                    .map_err(|e| os_message(&e))
            }
        };
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(message) => {
                had_error = true;
                let _ = writeln!(io.stderr, "{}: {label}: {message}", io.name);
                continue;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut matched = 0;
        for (index, line) in split_lines(&text).into_iter().enumerate() {
            if re.is_match(line) == invert {
                continue;
            }
            matched += 1;
            if list_only || count_only {
                continue;
            }
            let name = if with_name {
                format!("{label}:")
            } else {
                String::new()
            };
            let number = if line_numbers {
                format!("{}:", index + 1)
            } else {
                String::new()
            };
            let _ = writeln!(io.stdout, "{name}{number}{line}");
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
// sed (substitution only)
// ---------------------------------------------------------------------------------------------

/// One piece of a replacement: literal text or a capture group (`0` is the whole match).
enum Part {
    Text(String),
    Group(usize),
}

struct Subst {
    pattern: String,
    parts: Vec<Part>,
    global: bool,
    print: bool,
}

pub(super) fn sed(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "n", "e", false) {
        Ok(opts) => opts,
        Err(message) => return usage_error(io, &message),
    };
    let scripts = opts.values('e');
    let (script, files) = match scripts.as_slice() {
        [] => match opts.operands.split_first() {
            Some((script, rest)) => (script.as_str(), rest),
            None => return io.usage(),
        },
        [one] => (*one, opts.operands.as_slice()),
        _ => return unsupported_sed(io),
    };
    let Some(parsed) = parse_subst(script) else {
        return unsupported_sed(io);
    };
    let re = match Regex::new(&parsed.pattern) {
        Ok(re) => re,
        Err(error) => {
            let _ = writeln!(io.stderr, "{}: {error}", io.name);
            return 2;
        }
    };
    let max_ref = parsed
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Group(index) => Some(*index),
            Part::Text(_) => None,
        })
        .max()
        .unwrap_or(0);
    if max_ref >= re.captures_len() {
        return io.fail(format!(
            "invalid reference \\{max_ref} on `s' command's RHS"
        ));
    }

    let quiet = opts.has('n');
    let mut status = 0;
    for input in inputs(files) {
        let text = match read_text(io, input) {
            Ok(text) => text,
            Err(message) => {
                status = io.fail(message);
                continue;
            }
        };
        for line in split_lines(&text) {
            let (out, substituted) = match substitute(&re, &parsed.parts, parsed.global, line) {
                Some(replaced) => (replaced, true),
                None => (line.to_owned(), false),
            };
            if substituted && parsed.print {
                let _ = writeln!(io.stdout, "{out}");
            }
            if !quiet {
                let _ = writeln!(io.stdout, "{out}");
            }
        }
    }
    status
}

fn unsupported_sed(io: &mut Io<'_>) -> i32 {
    let _ = writeln!(io.stderr, "{}: unsupported sed script", io.name);
    2
}

/// Parses `s/RE/REPL/[gp]`. `\/` stands for a literal slash in both parts; other backslash
/// pairs are kept for the regex or the replacement parser.
fn parse_subst(script: &str) -> Option<Subst> {
    let rest = script.strip_prefix("s/")?;
    let mut chars = rest.chars();
    let pattern = scan_part(&mut chars)?;
    let replacement = scan_part(&mut chars)?;
    let (mut global, mut print) = (false, false);
    for flag in chars {
        match flag {
            'g' => global = true,
            'p' => print = true,
            _ => return None,
        }
    }
    Some(Subst {
        pattern,
        parts: parse_replacement(&replacement),
        global,
        print,
    })
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

/// Applies the substitution to one line. `None` when nothing matched.
fn substitute(re: &Regex, parts: &[Part], global: bool, line: &str) -> Option<String> {
    let mut out = String::new();
    let mut copied = 0;
    let mut found = false;
    for caps in re.captures_iter(line) {
        let whole = caps.get(0)?;
        found = true;
        out.push_str(&line[copied..whole.start()]);
        for part in parts {
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
        if !global {
            break;
        }
    }
    if !found {
        return None;
    }
    out.push_str(&line[copied..]);
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// sort
// ---------------------------------------------------------------------------------------------

pub(super) fn sort(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "rnu", "", false) {
        Ok(opts) => opts,
        Err(message) => return usage_error(io, &message),
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
        Err(message) => return usage_error(io, &message),
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
        Err(message) => return usage_error(io, &message),
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

// ---------------------------------------------------------------------------------------------
// xargs
// ---------------------------------------------------------------------------------------------

pub(super) fn xargs(io: &mut Io<'_>, args: &[String]) -> i32 {
    let opts = match parse_opts(args, "", "nI", true) {
        Ok(opts) => opts,
        Err(message) => return usage_error(io, &message),
    };
    let per_call = match opts.values('n').last().copied() {
        None => usize::MAX,
        Some(text) => match text.parse::<usize>() {
            Ok(count) if count > 0 => count,
            _ => return usage_error(io, &format!("invalid number of arguments '{text}'")),
        },
    };
    let replace = opts.values('I').last().copied();
    if replace == Some("") {
        return usage_error(io, "replacement string must not be empty");
    }
    // The command and its fixed arguments; with no operands, GNU xargs defaults to echo.
    let template: Vec<String> = if opts.operands.is_empty() {
        vec!["echo".to_owned()]
    } else {
        opts.operands.clone()
    };

    let mut input = Vec::new();
    if let Err(error) = io.stdin.read_to_end(&mut input) {
        return io.fail(format!("standard input: {}", os_message(&error)));
    }
    let text = String::from_utf8_lossy(&input);
    let invocations = match replace {
        Some(repl) => replace_args(&template, repl, &text),
        None => {
            let words: Vec<&str> = text.split_whitespace().collect();
            batch_args(&template, &words, per_call)
        }
    };

    let mut status = 0;
    for argv in &invocations {
        let output = match Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(io.cwd)
            .env_clear()
            .envs(io.env.iter().cloned())
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                let reason = if error.kind() == std::io::ErrorKind::NotFound {
                    "not found".to_owned()
                } else {
                    os_message(&error)
                };
                let _ = writeln!(io.stderr, "xargs: {}: {reason}", argv[0]);
                return 127;
            }
        };
        let _ = io.stdout.write_all(&output.stdout);
        let _ = io.stderr.write_all(&output.stderr);
        if !output.status.success() {
            status = 123;
        }
    }
    status
}

/// Builds the argument vectors (command first) for the default mode: the input words appended to
/// `template`, at most `per_call` words per invocation. With no words, one invocation runs with
/// just `template`, as GNU xargs does.
fn batch_args(template: &[String], words: &[&str], per_call: usize) -> Vec<Vec<String>> {
    if words.is_empty() {
        return vec![template.to_vec()];
    }
    words
        .chunks(per_call)
        .map(|chunk| {
            template
                .iter()
                .cloned()
                .chain(chunk.iter().map(|word| (*word).to_owned()))
                .collect()
        })
        .collect()
}

/// Builds the argument vectors for `-I` mode: one invocation per non-empty input line, with every
/// `repl` occurrence in each template word replaced by that line.
fn replace_args(template: &[String], repl: &str, input: &str) -> Vec<Vec<String>> {
    input
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            template
                .iter()
                .map(|word| word.replace(repl, line))
                .collect::<Vec<String>>()
        })
        .collect()
}

#[cfg(test)]
mod tests;

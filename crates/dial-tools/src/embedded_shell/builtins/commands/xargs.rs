//! `xargs`: builds argument lists from standard input and runs the command once per list through
//! the shell engine (see [`super::invoke`]).

use brush_core::ExecutionContext;

use super::super::Io;
use super::super::files::err_text;
use super::{Outcome, Stdin, failure_status, invoke, report_failure};

/// Parsed options and the command template (the words given before any input word).
#[derive(Debug, Default, PartialEq)]
struct Options {
    /// `-0`: input words are NUL-terminated and taken literally.
    null: bool,
    /// `-r`: run nothing when the input has no words.
    no_run_if_empty: bool,
    /// `-n N`: at most N input words per invocation.
    per_call: Option<usize>,
    /// `-I REPL`: one invocation per input record, with REPL replaced in each template word.
    replace: Option<String>,
    /// Command and fixed arguments; `echo` when none was given.
    template: Vec<String>,
}

/// Parses options up to the first operand, which starts the command (GNU xargs stops there).
fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        for (at, flag) in arg.char_indices().skip(1) {
            match flag {
                '0' => opts.null = true,
                'r' => opts.no_run_if_empty = true,
                'n' | 'I' => {
                    let attached = &arg[at + flag.len_utf8()..];
                    let value = if attached.is_empty() {
                        i += 1;
                        args.get(i)
                            .cloned()
                            .ok_or_else(|| format!("option requires an argument -- '{flag}'"))?
                    } else {
                        attached.to_owned()
                    };
                    if flag == 'n' {
                        let count = value
                            .parse::<usize>()
                            .ok()
                            .filter(|count| *count > 0)
                            .ok_or_else(|| format!("invalid number of arguments '{value}'"))?;
                        opts.per_call = Some(count);
                    } else if value.is_empty() {
                        return Err("replacement string must not be empty".to_owned());
                    } else {
                        opts.replace = Some(value);
                    }
                    break;
                }
                other => return Err(format!("invalid option -- '{other}'")),
            }
        }
        i += 1;
    }
    opts.template = args[i..].to_vec();
    if opts.template.is_empty() {
        opts.template.push("echo".to_owned());
    }
    Ok(opts)
}

/// Splits input on blanks (space, tab, newline) the way GNU xargs does: `'...'` is literal,
/// `"..."` keeps its bytes except that `\"` and `\\` yield the escaped byte, and a backslash
/// outside quotes escapes the next byte (so an escaped blank stays in its word). A trailing
/// backslash is dropped.
fn split_quoted(input: &[u8]) -> Result<Vec<String>, String> {
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut word: Option<Vec<u8>> = None;
    let mut bytes = input.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        match byte {
            b' ' | b'\t' | b'\n' => words.extend(word.take()),
            b'\'' => {
                let current = word.get_or_insert_with(Vec::new);
                loop {
                    match bytes.next() {
                        Some(b'\'') => break,
                        Some(c) => current.push(c),
                        None => return Err("unmatched single quote".to_owned()),
                    }
                }
            }
            b'"' => {
                let current = word.get_or_insert_with(Vec::new);
                loop {
                    match bytes.next() {
                        Some(b'"') => break,
                        Some(b'\\') => match bytes.peek().copied() {
                            Some(c @ (b'"' | b'\\')) => {
                                bytes.next();
                                current.push(c);
                            }
                            _ => current.push(b'\\'),
                        },
                        Some(c) => current.push(c),
                        None => return Err("unmatched double quote".to_owned()),
                    }
                }
            }
            b'\\' => {
                if let Some(c) = bytes.next() {
                    word.get_or_insert_with(Vec::new).push(c);
                }
            }
            c => word.get_or_insert_with(Vec::new).push(c),
        }
    }
    words.extend(word.take());
    Ok(words.into_iter().map(lossy).collect())
}

/// Splits `input` on `delim`, dropping the empty piece after a final delimiter.
fn records(input: &[u8], delim: u8) -> Vec<&[u8]> {
    if input.is_empty() {
        return Vec::new();
    }
    let mut pieces: Vec<&[u8]> = input.split(|byte| *byte == delim).collect();
    if input.last() == Some(&delim) {
        pieces.pop();
    }
    pieces
}

fn lossy(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

/// Builds the argument vectors (command first) for `input`. Without `-I`, the input words are
/// appended to the template, at most `-n` per invocation; with no words, one invocation runs
/// the bare template unless `-r` is given. With `-I`, each non-empty record is one invocation.
fn invocations(opts: &Options, input: &[u8]) -> Result<Vec<Vec<String>>, String> {
    if let Some(replace) = &opts.replace {
        let delim = if opts.null { b'\0' } else { b'\n' };
        return Ok(records(input, delim)
            .into_iter()
            .map(|record| {
                if opts.null {
                    record
                } else {
                    // GNU xargs ignores blanks at the start of an input line.
                    let start = record
                        .iter()
                        .position(|byte| !matches!(byte, b' ' | b'\t'))
                        .unwrap_or(record.len());
                    &record[start..]
                }
            })
            .filter(|record| !record.is_empty())
            .map(|record| {
                let text = lossy(record.to_vec());
                opts.template
                    .iter()
                    .map(|word| word.replace(replace.as_str(), &text))
                    .collect()
            })
            .collect());
    }
    let words: Vec<String> = if opts.null {
        records(input, b'\0')
            .into_iter()
            .map(|w| lossy(w.to_vec()))
            .collect()
    } else {
        split_quoted(input)?
    };
    if words.is_empty() {
        return Ok(if opts.no_run_if_empty {
            Vec::new()
        } else {
            vec![opts.template.clone()]
        });
    }
    let per_call = opts.per_call.unwrap_or(usize::MAX);
    Ok(words
        .chunks(per_call)
        .map(|chunk| {
            opts.template
                .iter()
                .cloned()
                .chain(chunk.iter().cloned())
                .collect()
        })
        .collect())
}

/// What xargs does after one invocation.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Go on to the next invocation; `failed` marks the run as failed (status 123 at the end).
    Next { failed: bool },
    /// Stop now and exit with this status.
    Stop(i32),
}

/// Maps one invocation's outcome to the next step, as GNU xargs does. A command that exits 125
/// itself is an ordinary failure; only a stopped command ends the run with 125.
fn step(outcome: Outcome) -> Step {
    match outcome {
        Outcome::Exited(0) => Step::Next { failed: false },
        Outcome::Exited(255) => Step::Stop(124),
        Outcome::Exited(code @ (126 | 127)) => Step::Stop(i32::from(code)),
        Outcome::Exited(_) => Step::Next { failed: true },
        Outcome::Stopped => Step::Stop(125),
    }
}

/// `xargs [-0] [-r] [-n N] [-I REPL] [COMMAND [ARG]...]`.
///
/// Exit status follows GNU xargs: 0 when every invocation exits 0; 123 when an invocation exits
/// 1-125; 124 when one exits 255 (xargs stops); 127 or 126 when the command cannot be run (xargs
/// stops); 125 when a command stops.
pub(in super::super) async fn xargs(
    io: &mut Io<'_>,
    ctx: &mut ExecutionContext<'_>,
    args: &[String],
) -> i32 {
    let opts = match parse_options(args) {
        Ok(opts) => opts,
        Err(message) => return io.decline(message),
    };
    let mut input = Vec::new();
    if let Err(error) = io.stdin.read_to_end(&mut input) {
        return io.fail(format!("standard input: {}", err_text(&error)));
    }
    let plan = match invocations(&opts, &input) {
        Ok(plan) => plan,
        Err(message) => return io.fail(message),
    };
    let mut status = 0;
    for argv in &plan {
        match invoke(ctx, argv, Stdin::Null).await {
            Ok(outcome) => match step(outcome) {
                Step::Next { failed } => status = if failed { 123 } else { status },
                Step::Stop(code) => return code,
            },
            Err(error) => {
                report_failure(io, &argv[0], &error);
                return failure_status(&error);
            }
        }
    }
    status
}

#[cfg(test)]
mod tests;

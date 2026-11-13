//! Stream commands for the built-in shell: `tee` and `seq`.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};

use super::files::{err_text, parse_flags};
use super::{Io, usage_error};

/// `tee [-a] [FILE]...`: copies standard input to standard output and to each file. A file that
/// cannot be opened or written is reported and skipped; the others still receive the input, and the
/// exit status is 1.
pub(super) fn tee(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (flags, names) = match parse_flags(args, "a") {
        Ok(parsed) => parsed,
        Err(message) => return io.decline(message),
    };
    let append = flags.contains('a');
    let mut status = 0;
    let mut files: Vec<(String, File)> = Vec::new();
    for name in names {
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        if append {
            options.append(true);
        } else {
            options.truncate(true);
        }
        match options.open(io.path(name)) {
            Ok(file) => files.push((name.to_owned(), file)),
            Err(error) => status = io.fail(format!("{name}: {}", err_text(&error))),
        }
    }
    let mut buffer = [0u8; 8192];
    loop {
        let read = match io.stdin.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                status = io.fail(format!("read error: {}", err_text(&error)));
                break;
            }
        };
        let chunk = &buffer[..read];
        if let Err(error) = io.stdout.write_all(chunk) {
            return io.fail(format!("standard output: {}", err_text(&error)));
        }
        let mut index = 0;
        while index < files.len() {
            match files[index].1.write_all(chunk) {
                Ok(()) => index += 1,
                Err(error) => {
                    let (name, _) = files.remove(index);
                    status = io.fail(format!("{name}: {}", err_text(&error)));
                }
            }
        }
    }
    if let Err(error) = io.stdout.flush() {
        status = io.fail(format!("standard output: {}", err_text(&error)));
    }
    status
}

/// A number operand of `seq`: finite, so every computed value is finite too.
fn parse_number(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// Digits after the decimal point of an operand, which set the output precision (`0.5` prints one
/// decimal place). Exponent forms are counted by their mantissa only.
fn decimals(text: &str) -> usize {
    let mantissa = text.split(['e', 'E']).next().unwrap_or_default();
    mantissa.split_once('.').map_or(0, |(_, frac)| frac.len())
}

/// Left-pads the digits of `text` with zeros to `width`, keeping a leading minus sign in front.
fn zero_pad(text: &str, width: usize) -> String {
    let sign = if text.starts_with('-') { "-" } else { "" };
    let digits = &text[sign.len()..];
    format!("{sign}{digits:0>w$}", w = width.saturating_sub(sign.len()))
}

/// `seq [-w] [-s SEP] [FIRST [INCR]] LAST`: prints numbers from FIRST by INCR up to LAST, one per
/// line unless a separator is given. Operands are integers or decimals.
pub(super) fn seq(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut separator = "\n".to_owned();
    let mut equal_width = false;
    let mut operands: Vec<String> = Vec::new();
    let mut words = args.iter();
    let mut literal = false;
    while let Some(arg) = words.next() {
        // A negative number is an operand (`seq -2 2`), never an option.
        if literal || !arg.starts_with('-') || arg == "-" || parse_number(arg).is_some() {
            operands.push(arg.clone());
            continue;
        }
        if arg == "--" {
            literal = true;
            continue;
        }
        for (at, flag) in arg[1..].char_indices() {
            match flag {
                'w' => equal_width = true,
                's' => {
                    let rest = &arg[1 + at + 1..];
                    let value = if rest.is_empty() {
                        match words.next() {
                            Some(value) => value.clone(),
                            None => return io.decline("option requires an argument -- 's'"),
                        }
                    } else {
                        rest.to_owned()
                    };
                    separator = value;
                    break;
                }
                _ => return io.decline(format!("invalid option -- '{flag}'")),
            }
        }
    }
    let texts: Vec<&str> = operands.iter().map(String::as_str).collect();
    let (start_text, step_text, end_text) = match texts.as_slice() {
        [end] => ("1", "1", *end),
        [start, end] => (*start, "1", *end),
        [start, step, end] => (*start, *step, *end),
        _ => return usage_error(io, "wrong number of operands"),
    };
    let Some(start) = parse_number(start_text) else {
        return io.fail(format!("invalid floating point argument: '{start_text}'"));
    };
    let Some(increment) = parse_number(step_text) else {
        return io.fail(format!("invalid floating point argument: '{step_text}'"));
    };
    let Some(end) = parse_number(end_text) else {
        return io.fail(format!("invalid floating point argument: '{end_text}'"));
    };
    if increment == 0.0 {
        return io.fail(format!("invalid Zero increment value: '{step_text}'"));
    }
    let precision = [start_text, step_text, end_text]
        .iter()
        .map(|text| decimals(text))
        .max()
        .unwrap_or(0);
    let span = (end - start) / increment;
    if !span.is_finite() {
        return io.fail("value out of range");
    }
    if span < 0.0 {
        return 0;
    }
    // A small tolerance keeps the last value of decimal steps (0.1 steps reach 1.0).
    let count = (span + 1e-9).floor() as u64;
    let value_at = |index: u64| format!("{:.*}", precision, start + index as f64 * increment);
    let width = if equal_width {
        (0..=count)
            .map(|index| value_at(index).len())
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let mut out = BufWriter::new(&mut *io.stdout);
    for index in 0..=count {
        if index > 0 && write!(out, "{separator}").is_err() {
            return 1;
        }
        let text = value_at(index);
        let text = if equal_width {
            zero_pad(&text, width)
        } else {
            text
        };
        if write!(out, "{text}").is_err() {
            return 1;
        }
    }
    if count > 0 && writeln!(out).is_err() {
        return 1;
    }
    let flushed = out.flush();
    drop(out);
    match flushed {
        Ok(()) => 0,
        Err(error) => io.fail(format!("standard output: {}", err_text(&error))),
    }
}

#[cfg(test)]
mod tests;

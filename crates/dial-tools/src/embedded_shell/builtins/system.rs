//! Host-facing commands for the built-in shell: `sleep`, `which`, `mktemp`, `realpath` and
//! `kill` (brush's `kill` exists only on Unix and defaults to `KILL`; see [`kill`]).
//!
//! Options outside the documented subsets are usage errors (exit 2), never ignored.

mod kill;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
#[cfg(any(windows, test))]
use std::time::Duration;

use dial_core::platform::fs::executable_candidate;

use super::files::{err_text, parse_flags};
use super::{Io, usage_error};

pub(super) use kill::kill;

/// Parses one `sleep` operand: a number with an optional `s`, `m`, `h` or `d` suffix. Only finite,
/// non-negative values are accepted.
#[cfg(any(windows, test))]
fn parse_interval(text: &str) -> Option<Duration> {
    let (number, unit) = match text.chars().last() {
        Some(c @ ('s' | 'm' | 'h' | 'd')) => {
            let scale = match c {
                's' => 1.0,
                'm' => 60.0,
                'h' => 3_600.0,
                _ => 86_400.0,
            };
            (&text[..text.len() - 1], scale)
        }
        Some(c) if c.is_ascii_alphabetic() => return None,
        _ => (text, 1.0),
    };
    let value: f64 = number.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(value * unit).ok()
}

/// `sleep NUMBER[SUFFIX]...`: waits for the sum of the operands. Async so that an aborted shell
/// run cancels the wait instead of holding the batch for its full length. Windows only (see
/// the module docs); the tests exercise it on every platform.
#[cfg(any(windows, test))]
pub(super) async fn sleep(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut total = Duration::ZERO;
    let mut operands = 0;
    let mut literal = false;
    for arg in args {
        if !literal && arg == "--" {
            literal = true;
            continue;
        }
        if !literal && arg.len() > 1 && arg.starts_with('-') {
            return io.decline(format!("invalid option -- '{}'", &arg[1..]));
        }
        let Some(interval) = parse_interval(arg) else {
            return io.fail(format!("invalid time interval '{arg}'"));
        };
        let Some(sum) = total.checked_add(interval) else {
            return io.fail(format!("invalid time interval '{arg}'"));
        };
        total = sum;
        operands += 1;
    }
    if operands == 0 {
        return usage_error(io, "missing operand");
    }
    tokio::time::sleep(total).await;
    0
}

/// The value of exported variable `name`, from the shell environment passed to the command.
fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Every executable `name` on `path`, in search order (only the first unless `all`). Relative
/// `PATH` entries resolve against `cwd`; `PATHEXT` suffixes come from [`executable_candidate`]. A
/// name with a separator is checked where it points, as the shell does.
fn find_all(name: &str, path: &OsStr, cwd: &Path, all: bool) -> Vec<PathBuf> {
    let has_separator = name.contains('/') || (cfg!(windows) && name.contains('\\'));
    if has_separator {
        let check = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            cwd.join(name)
        };
        return executable_candidate(&check)
            .map(|_| PathBuf::from(name))
            .into_iter()
            .collect();
    }
    let mut found = Vec::new();
    for dir in std::env::split_paths(path) {
        let resolved = if dir.is_relative() {
            cwd.join(&dir)
        } else {
            dir.clone()
        };
        if let Some(candidate) = executable_candidate(&resolved.join(name))
            && let Some(file) = candidate.file_name()
        {
            found.push(dir.join(file));
            if !all {
                break;
            }
        }
    }
    found
}

/// `which [-a] NAME...`: prints the file each name resolves to on `PATH`. Exit 1 when any name is
/// not found. Only `PATH` is searched; bundled utilities are not reported.
pub(super) fn which(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (flags, names) = match parse_flags(args, "a") {
        Ok(parsed) => parsed,
        Err(message) => return io.decline(message),
    };
    if names.is_empty() {
        return usage_error(io, "missing operand");
    }
    let all = flags.contains('a');
    let path = env_value(io.env, "PATH").unwrap_or_default();
    let search = OsString::from(path);
    let cwd = io.cwd.to_path_buf();
    let mut status = 0;
    for name in names {
        let found = find_all(name, &search, &cwd, all);
        if found.is_empty() {
            status = io.fail(format!("no {name} in ({path})"));
        }
        for file in found {
            let _ = writeln!(io.stdout, "{}", file.display());
        }
    }
    status
}

/// Resolves `.` and `..` lexically. `path` is absolute here, so `..` never escapes the root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The longest existing prefix of `path`, canonicalized, with the missing rest appended.
fn resolve_missing(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut missing: Vec<&OsStr> = Vec::new();
    let mut existing = path.as_path();
    loop {
        if let Ok(real) = fs::canonicalize(existing) {
            let mut out = real;
            for part in missing.iter().rev() {
                out.push(part);
            }
            return normalize(&out);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return path,
        }
    }
}

/// Strips the `\\?\` verbatim prefix that `fs::canonicalize` adds on Windows.
fn plain(path: PathBuf) -> PathBuf {
    if cfg!(windows)
        && let Some(text) = path.as_os_str().to_str()
    {
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(local) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(local);
        }
    }
    path
}

/// How much of the path must exist, per `realpath`'s `-e` and `-m` options.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Existence {
    /// Every component but the last must exist (GNU default).
    AllButLast,
    /// Every component must exist (`-e`).
    All,
    /// Nothing needs to exist (`-m`).
    None,
}

fn resolve(path: &Path, existence: Existence) -> io::Result<PathBuf> {
    match existence {
        Existence::None => Ok(plain(resolve_missing(path))),
        Existence::All => fs::canonicalize(path).map(plain),
        Existence::AllButLast => match fs::canonicalize(path) {
            Ok(real) => Ok(plain(real)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                    return Err(error);
                };
                Ok(plain(fs::canonicalize(parent)?.join(name)))
            }
            Err(error) => Err(error),
        },
    }
}

/// `realpath [-e|-m] PATH...`: prints each path made absolute with symbolic links resolved.
pub(super) fn realpath(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (flags, operands) = match parse_flags(args, "em") {
        Ok(parsed) => parsed,
        Err(message) => return io.decline(message),
    };
    if flags.contains('e') && flags.contains('m') {
        return usage_error(io, "options -e and -m are mutually exclusive");
    }
    if operands.is_empty() {
        return usage_error(io, "missing operand");
    }
    let existence = match flags.chars().last() {
        Some('e') => Existence::All,
        Some('m') => Existence::None,
        _ => Existence::AllButLast,
    };
    let mut status = 0;
    for operand in operands {
        match resolve(&io.path(operand), existence) {
            Ok(path) => {
                let _ = writeln!(io.stdout, "{}", path.display());
            }
            Err(error) => {
                status = io.fail(format!("{operand}: {}", err_text(&error)));
            }
        }
    }
    status
}

/// Directory for `mktemp` when no template names one: `TMPDIR`, then `TEMP`/`TMP`, then the
/// platform temporary directory.
fn temp_dir(io: &Io<'_>) -> PathBuf {
    ["TMPDIR", "TEMP", "TMP"]
        .iter()
        .find_map(|name| env_value(io.env, name).filter(|value| !value.is_empty()))
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

/// Splits a template into its directory part (up to the last separator) and the file-name part.
fn split_template(template: &str) -> (&str, &str) {
    let separator = |c: char| c == '/' || (cfg!(windows) && c == '\\');
    match template.rfind(separator) {
        Some(i) => (&template[..=i], &template[i + 1..]),
        None => ("", template),
    }
}

/// `mktemp [-d] [-p DIR] [TEMPLATE]`: creates a unique file (or directory with `-d`) and prints its
/// name. The template's trailing `X`s (at least three) become random characters; the file is
/// created exclusively, so an existing path is never reused.
pub(super) fn mktemp(io: &mut Io<'_>, args: &[String]) -> i32 {
    let mut directory = false;
    let mut parent: Option<&str> = None;
    let mut operands: Vec<&str> = Vec::new();
    let mut literal = false;
    let mut words = args.iter();
    while let Some(arg) = words.next() {
        if literal || !arg.starts_with('-') || arg == "-" {
            operands.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => literal = true,
            "-d" => directory = true,
            "-p" => match words.next() {
                Some(dir) => parent = Some(dir),
                None => return io.decline("option requires an argument -- 'p'"),
            },
            _ => return io.decline(format!("invalid option -- '{}'", &arg[1..])),
        }
    }
    if operands.len() > 1 {
        return usage_error(io, "too many templates");
    }
    let template = operands.first().copied().unwrap_or("tmp.XXXXXXXXXX");
    let count = template.chars().rev().take_while(|c| *c == 'X').count();
    if count < 3 {
        return io.fail(format!("too few X's in template '{template}'"));
    }
    let (dir_part, name_part) = split_template(&template[..template.len() - count]);
    let base = match (parent, dir_part.is_empty(), operands.is_empty()) {
        (Some(_), false, _) => {
            return io.fail(format!(
                "invalid template '{template}', contains directory separator"
            ));
        }
        (_, false, _) => io.path(dir_part),
        (Some(dir), true, _) => io.path(dir),
        (None, true, true) => temp_dir(io),
        (None, true, false) => io.cwd.to_path_buf(),
    };
    let created = if directory {
        tempfile::Builder::new()
            .prefix(name_part)
            .rand_bytes(count)
            .tempdir_in(&base)
            .map(tempfile::TempDir::keep)
            .map_err(|error| error.to_string())
    } else {
        tempfile::Builder::new()
            .prefix(name_part)
            .rand_bytes(count)
            .tempfile_in(&base)
            .map_err(|error| error.to_string())
            .and_then(|file| file.keep().map(|(_, path)| path).map_err(|e| e.to_string()))
    };
    match created {
        Ok(path) => {
            let _ = writeln!(io.stdout, "{}", path.display());
            0
        }
        Err(reason) => io.fail(format!("failed to create '{template}': {reason}")),
    }
}

#[cfg(test)]
mod tests;

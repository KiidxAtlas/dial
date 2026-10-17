//! File and path coreutils for the built-in shell: `basename`, `cat`, `cp`, `dirname`, `find`,
//! `head`, `ls`, `mkdir`, `mv`, `rm`, `tail`, `touch`, `wc`.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::Io;

/// Text of an I/O error without Rust's ` (os error N)` suffix, matching coreutils messages.
fn err_text(e: &io::Error) -> String {
    let text = e.to_string();
    match text.find(" (os error") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

/// Writes `name: message` for an I/O error and returns exit code 1.
fn report(io: &mut Io<'_>, name: &str, e: &io::Error) -> i32 {
    io.fail(format!("{name}: {}", err_text(e)))
}

/// Splits arguments into the single-letter flags drawn from `allowed` and the operands.
/// Returns `None` on an unknown flag. `-` and everything after `--` are operands.
fn parse_flags<'a>(args: &'a [String], allowed: &str) -> Option<(String, Vec<&'a str>)> {
    let mut flags = String::new();
    let mut operands = Vec::new();
    let mut literal = false;
    for arg in args {
        let arg = arg.as_str();
        if literal || !arg.starts_with('-') || arg == "-" {
            operands.push(arg);
        } else if arg == "--" {
            literal = true;
        } else {
            for c in arg[1..].chars() {
                if !allowed.contains(c) {
                    return None;
                }
                flags.push(c);
            }
        }
    }
    Some((flags, operands))
}

/// Parses `[-n N] [FILE]...` for head and tail. `N` defaults to 10.
fn line_count_args(args: &[String]) -> Option<(usize, Vec<&str>)> {
    let mut count: usize = 10;
    let mut operands = Vec::new();
    let mut literal = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let arg = arg.as_str();
        if literal || !arg.starts_with('-') || arg == "-" {
            operands.push(arg);
        } else if arg == "--" {
            literal = true;
        } else if arg == "-n" {
            count = iter.next()?.parse().ok()?;
        } else {
            let value = arg.strip_prefix("-n")?;
            count = value.parse().ok()?;
        }
    }
    Some((count, operands))
}

/// Last component of `path`, ignoring trailing slashes. `/` and `//` yield `/`.
fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        if path.is_empty() { "" } else { "/" }
    } else {
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    }
}

/// GNU `dirname` for one name.
fn dirname_of(name: &str) -> &str {
    let trimmed = name.trim_end_matches('/');
    if trimmed.is_empty() {
        return if name.is_empty() { "." } else { "/" };
    }
    match trimmed.rfind('/') {
        None => ".",
        Some(i) => {
            let dir = trimmed[..i].trim_end_matches('/');
            if dir.is_empty() { "/" } else { dir }
        }
    }
}

/// Writes the `==> NAME <==` header used when several inputs are given, separated by a blank line.
fn header(out: &mut dyn Write, multi: bool, printed: &mut bool, label: &str) {
    if !multi {
        return;
    }
    if *printed {
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "==> {label} <==");
    *printed = true;
}

/// Runs `each` over every operand in order; `-` or no operands mean stdin. Errors are reported
/// per input and the exit code is 1 if any input failed.
fn for_each_input<F>(io: &mut Io<'_>, operands: &[&str], headers: bool, mut each: F) -> i32
where
    F: FnMut(&mut dyn BufRead, &mut dyn Write) -> io::Result<()>,
{
    let names: Vec<&str> = if operands.is_empty() {
        vec!["-"]
    } else {
        operands.to_vec()
    };
    let multi = headers && names.len() > 1;
    let mut status = 0;
    let mut printed = false;
    for name in names {
        let label = if name == "-" { "standard input" } else { name };
        let outcome = if name == "-" {
            header(&mut *io.stdout, multi, &mut printed, label);
            each(&mut BufReader::new(&mut *io.stdin), &mut *io.stdout)
        } else {
            match File::open(io.path(name)) {
                Ok(file) => {
                    header(&mut *io.stdout, multi, &mut printed, label);
                    each(&mut BufReader::new(file), &mut *io.stdout)
                }
                Err(e) => {
                    status = report(io, name, &e);
                    continue;
                }
            }
        };
        if let Err(e) = outcome {
            status = report(io, name, &e);
        }
    }
    status
}

pub(super) fn basename(io: &mut Io<'_>, args: &[String]) -> i32 {
    if !(1..=2).contains(&args.len()) {
        return io.usage();
    }
    let mut name = base_name(&args[0]);
    if let Some(suffix) = args.get(1)
        && !suffix.is_empty()
        && name.len() > suffix.len()
        && name.ends_with(suffix.as_str())
    {
        name = &name[..name.len() - suffix.len()];
    }
    let _ = writeln!(io.stdout, "{name}");
    0
}

pub(super) fn dirname(io: &mut Io<'_>, args: &[String]) -> i32 {
    if args.is_empty() {
        return io.usage();
    }
    for name in args {
        let _ = writeln!(io.stdout, "{}", dirname_of(name));
    }
    0
}

fn cat_lines(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    number: bool,
    line_no: &mut usize,
) -> io::Result<()> {
    if !number {
        io::copy(input, out)?;
        return Ok(());
    }
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        *line_no += 1;
        write!(out, "{:>6}\t", line_no)?;
        out.write_all(&line)?;
    }
}

pub(super) fn cat(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((flags, operands)) = parse_flags(args, "n") else {
        return io.usage();
    };
    let number = flags.contains('n');
    let mut line_no = 0;
    for_each_input(io, &operands, false, |input, out| {
        cat_lines(input, out, number, &mut line_no)
    })
}

/// Copies `src` to `dst`. Inside a directory tree, symlinks are recreated as links on unix;
/// `top` sources are always followed. Directories are copied recursively.
fn copy_path(src: &Path, dst: &Path, top: bool) -> io::Result<()> {
    let follow = top || !cfg!(unix);
    let meta = if follow {
        fs::metadata(src)?
    } else {
        fs::symlink_metadata(src)?
    };
    #[cfg(unix)]
    {
        if meta.file_type().is_symlink() {
            return std::os::unix::fs::symlink(fs::read_link(src)?, dst);
        }
    }
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_path(&entry.path(), &dst.join(entry.file_name()), false)?;
        }
        Ok(())
    } else {
        if let (Ok(a), Ok(b)) = (fs::canonicalize(src), fs::canonicalize(dst))
            && a == b
        {
            return Err(io::Error::other("are the same file"));
        }
        fs::copy(src, dst).map(|_| ())
    }
}

/// Destination path for `source`: inside `dest` when it is a directory, else `dest` itself.
fn target_for(dest: &Path, dest_is_dir: bool, source: &str) -> PathBuf {
    if dest_is_dir {
        dest.join(base_name(source))
    } else {
        dest.to_path_buf()
    }
}

pub(super) fn cp(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((flags, operands)) = parse_flags(args, "r") else {
        return io.usage();
    };
    let Some((dest, sources)) = operands.split_last().filter(|(_, rest)| !rest.is_empty()) else {
        return io.usage();
    };
    let recursive = flags.contains('r');
    let dest_path = io.path(dest);
    let dest_is_dir = dest_path.is_dir();
    if sources.len() > 1 && !dest_is_dir {
        return io.fail(format!("target '{dest}' is not a directory"));
    }
    let mut status = 0;
    for &source in sources {
        let src_path = io.path(source);
        if !recursive && src_path.is_dir() {
            status = io.fail(format!(
                "{source}: -r not specified; omitting directory '{source}'"
            ));
            continue;
        }
        let target = target_for(&dest_path, dest_is_dir, source);
        if let Err(e) = copy_path(&src_path, &target, true) {
            status = report(io, source, &e);
        }
    }
    status
}

/// Removes a file, symlink, or directory tree at `path`.
fn remove_path(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Renames `src` to `dst`, falling back to copy-then-remove across devices.
fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_path(src, dst, false)?;
            remove_path(src)
        }
        other => other,
    }
}

pub(super) fn mv(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((dest, sources)) = args.split_last().filter(|(_, rest)| !rest.is_empty()) else {
        return io.usage();
    };
    let dest_path = io.path(dest);
    let dest_is_dir = dest_path.is_dir();
    if sources.len() > 1 && !dest_is_dir {
        return io.fail(format!("target '{dest}' is not a directory"));
    }
    let mut status = 0;
    for source in sources {
        let target = target_for(&dest_path, dest_is_dir, source);
        if let Err(e) = move_path(&io.path(source), &target) {
            status = report(io, source, &e);
        }
    }
    status
}

pub(super) fn mkdir(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((flags, operands)) = parse_flags(args, "p") else {
        return io.usage();
    };
    if operands.is_empty() {
        return io.usage();
    }
    let parents = flags.contains('p');
    let mut status = 0;
    for name in operands {
        let path = io.path(name);
        let made = if parents {
            fs::create_dir_all(&path)
        } else {
            fs::create_dir(&path)
        };
        if let Err(e) = made {
            status = report(io, name, &e);
        }
    }
    status
}

pub(super) fn rm(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((flags, operands)) = parse_flags(args, "rf") else {
        return io.usage();
    };
    let recursive = flags.contains('r');
    let force = flags.contains('f');
    if operands.is_empty() {
        return if force { 0 } else { io.usage() };
    }
    let mut status = 0;
    for name in operands {
        let path = io.path(name);
        let result = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() && !recursive => Err(io::Error::other("Is a directory")),
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(e) => Err(e),
        };
        if let Err(e) = result
            && !(force && e.kind() == io::ErrorKind::NotFound)
        {
            status = report(io, name, &e);
        }
    }
    status
}

pub(super) fn touch(io: &mut Io<'_>, args: &[String]) -> i32 {
    if args.is_empty() {
        return io.usage();
    }
    let mut status = 0;
    for name in args {
        let path = io.path(name);
        let result = if path.exists() {
            File::open(&path).and_then(|file| file.set_modified(SystemTime::now()))
        } else {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map(|_| ())
        };
        if let Err(e) = result {
            status = report(io, name, &e);
        }
    }
    status
}

pub(super) fn head(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((count, operands)) = line_count_args(args) else {
        return io.usage();
    };
    for_each_input(io, &operands, true, |input, out| {
        let mut line = Vec::new();
        for _ in 0..count {
            line.clear();
            if input.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            out.write_all(&line)?;
        }
        Ok(())
    })
}

pub(super) fn tail(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((count, operands)) = line_count_args(args) else {
        return io.usage();
    };
    for_each_input(io, &operands, true, |input, out| {
        if count == 0 {
            return Ok(());
        }
        // Lines are kept verbatim, so a final line without a newline is written back without one.
        let mut kept: VecDeque<Vec<u8>> = VecDeque::new();
        loop {
            let mut line = Vec::new();
            if input.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            kept.push_back(line);
            if kept.len() > count {
                kept.pop_front();
            }
        }
        for line in &kept {
            out.write_all(line)?;
        }
        Ok(())
    })
}

/// Type letter for `ls -l`: `d` directory, `l` symlink, `-` file, `?` anything else.
fn type_char(meta: &fs::Metadata) -> char {
    let kind = meta.file_type();
    if kind.is_dir() {
        'd'
    } else if kind.is_symlink() {
        'l'
    } else if kind.is_file() {
        '-'
    } else {
        '?'
    }
}

fn write_entry(out: &mut dyn Write, path: &Path, shown: &str, long: bool) -> io::Result<()> {
    if !long {
        return writeln!(out, "{shown}");
    }
    let meta = fs::symlink_metadata(path)?;
    writeln!(out, "{} {:>10} {shown}", type_char(&meta), meta.len())
}

fn list_dir(out: &mut dyn Write, dir: &Path, all: bool, long: bool) -> io::Result<()> {
    let mut names: Vec<OsString> = Vec::new();
    for entry in fs::read_dir(dir)? {
        names.push(entry?.file_name());
    }
    names.retain(|name| all || !name.to_string_lossy().starts_with('.'));
    if all {
        names.push(OsString::from("."));
        names.push(OsString::from(".."));
    }
    names.sort();
    for name in names {
        let shown = name.to_string_lossy();
        write_entry(out, &dir.join(&name), &shown, long)?;
    }
    Ok(())
}

pub(super) fn ls(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((flags, mut operands)) = parse_flags(args, "al") else {
        return io.usage();
    };
    let all = flags.contains('a');
    let long = flags.contains('l');
    if operands.is_empty() {
        operands.push(".");
    }
    let multi = operands.len() > 1;
    let mut status = 0;
    let mut printed = false;
    for operand in operands {
        let path = io.path(operand);
        match fs::metadata(&path) {
            Err(e) => status = report(io, operand, &e),
            Ok(meta) if meta.is_dir() => {
                if multi {
                    if printed {
                        let _ = writeln!(io.stdout);
                    }
                    let _ = writeln!(io.stdout, "{operand}:");
                }
                printed = true;
                if let Err(e) = list_dir(&mut *io.stdout, &path, all, long) {
                    status = report(io, operand, &e);
                }
            }
            Ok(_) => {
                if let Err(e) = write_entry(&mut *io.stdout, &path, operand, long) {
                    status = report(io, operand, &e);
                }
            }
        }
    }
    status
}

/// Expression state for one `find` run, plus the program name used in error messages.
struct Find<'a> {
    prog: &'a str,
    name: Option<&'a str>,
    kind: Option<char>,
    max_depth: Option<usize>,
}

/// Joins a displayed parent path and a child name with a single `/`.
fn join_display(parent: &str, child: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{child}")
    } else {
        format!("{parent}/{child}")
    }
}

impl Find<'_> {
    /// Prints `shown` if it matches, then descends into directories (never through symlinks).
    fn visit(
        &self,
        out: &mut dyn Write,
        err: &mut dyn Write,
        path: &Path,
        shown: &str,
        depth: usize,
    ) -> i32 {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) => {
                let _ = writeln!(err, "{}: '{shown}': {}", self.prog, err_text(&e));
                return 1;
            }
        };
        let kind_ok = match self.kind {
            Some('f') => meta.is_file(),
            Some(_) => meta.is_dir(),
            None => true,
        };
        let name_ok = self
            .name
            .is_none_or(|glob| glob_match(glob, base_name(shown)));
        if kind_ok && name_ok {
            let _ = writeln!(out, "{shown}");
        }
        let mut status = 0;
        if meta.is_dir() && self.max_depth.is_none_or(|max| depth < max) {
            match fs::read_dir(path) {
                Ok(entries) => {
                    let mut names: Vec<OsString> = entries
                        .filter_map(Result::ok)
                        .map(|e| e.file_name())
                        .collect();
                    names.sort();
                    for child in names {
                        let child_name = child.to_string_lossy();
                        let child_shown = join_display(shown, &child_name);
                        status |= self.visit(out, err, &path.join(&child), &child_shown, depth + 1);
                    }
                }
                Err(e) => {
                    let _ = writeln!(err, "{}: '{shown}': {}", self.prog, err_text(&e));
                    status = 1;
                }
            }
        }
        status
    }
}

pub(super) fn find(io: &mut Io<'_>, args: &[String]) -> i32 {
    let (root, rest) = match args.first() {
        Some(first) if !first.starts_with('-') => (first.as_str(), &args[1..]),
        _ => (".", args),
    };
    let mut walk = Find {
        prog: io.name,
        name: None,
        kind: None,
        max_depth: None,
    };
    let mut i = 0;
    while i < rest.len() {
        let value = rest.get(i + 1).map(String::as_str);
        match (rest[i].as_str(), value) {
            ("-name", Some(glob)) => walk.name = Some(glob),
            ("-type", Some(t @ ("f" | "d"))) => walk.kind = t.chars().next(),
            ("-maxdepth", Some(n)) => match n.parse::<usize>() {
                Ok(depth) => walk.max_depth = Some(depth),
                Err(_) => return io.usage(),
            },
            _ => return io.usage(),
        }
        i += 2;
    }
    let root_path = io.path(root);
    walk.visit(&mut *io.stdout, &mut *io.stderr, &root_path, root, 0)
}

/// Shell-style glob over one file name: `*`, `?`, `[...]` (with `!`/`^` negation and ranges),
/// and `\` escapes.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = name.chars().collect();
    glob_at(&pat, &text)
}

fn glob_at(pat: &[char], text: &[char]) -> bool {
    match pat.first().copied() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).any(|skip| glob_at(&pat[1..], &text[skip..])),
        Some('?') => !text.is_empty() && glob_at(&pat[1..], &text[1..]),
        Some('[') if !text.is_empty() => match class_match(pat, text[0]) {
            Some((matched, rest)) => matched && glob_at(rest, &text[1..]),
            None => text[0] == '[' && glob_at(&pat[1..], &text[1..]),
        },
        Some('\\') if pat.len() > 1 => {
            !text.is_empty() && text[0] == pat[1] && glob_at(&pat[2..], &text[1..])
        }
        Some(c) => !text.is_empty() && text[0] == c && glob_at(&pat[1..], &text[1..]),
    }
}

/// Matches `c` against the bracket expression starting at `pat[0] == '['`. Returns whether it
/// matched and the pattern after the closing `]`, or `None` if the bracket is unterminated.
fn class_match(pat: &[char], c: char) -> Option<(bool, &[char])> {
    let mut i = 1;
    let negated = matches!(pat.get(i).copied(), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == ']' && !first {
            return Some((matched != negated, &pat[i + 1..]));
        }
        first = false;
        if i + 2 < pat.len() && pat[i + 1] == '-' && pat[i + 2] != ']' {
            matched |= (pat[i]..=pat[i + 2]).contains(&c);
            i += 3;
        } else {
            matched |= pat[i] == c;
            i += 1;
        }
    }
    None
}

#[derive(Clone, Copy, Default)]
struct Counts {
    lines: u64,
    words: u64,
    bytes: u64,
}

impl Counts {
    fn add(&mut self, other: Counts) {
        self.lines += other.lines;
        self.words += other.words;
        self.bytes += other.bytes;
    }
}

/// Streams `input` once, counting newlines, whitespace-separated words, and bytes.
fn count_stream(input: &mut dyn Read) -> io::Result<Counts> {
    let mut counts = Counts::default();
    let mut in_word = false;
    let mut buf = [0u8; 8192];
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => return Ok(counts),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for &byte in &buf[..n] {
            counts.bytes += 1;
            if byte == b'\n' {
                counts.lines += 1;
            }
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                in_word = false;
            } else if !in_word {
                in_word = true;
                counts.words += 1;
            }
        }
    }
}

/// Formats the selected columns (lines, words, bytes), each right-aligned to width 7.
fn wc_fields(counts: Counts, show: [bool; 3]) -> String {
    [counts.lines, counts.words, counts.bytes]
        .iter()
        .zip(show)
        .filter(|(_, on)| *on)
        .map(|(n, _)| format!("{n:>7}"))
        .collect()
}

pub(super) fn wc(io: &mut Io<'_>, args: &[String]) -> i32 {
    let Some((flags, operands)) = parse_flags(args, "lwc") else {
        return io.usage();
    };
    let mut show = [
        flags.contains('l'),
        flags.contains('w'),
        flags.contains('c'),
    ];
    if !show.contains(&true) {
        show = [true; 3];
    }
    let named = !operands.is_empty();
    let names: Vec<&str> = if named { operands } else { vec!["-"] };
    let mut total = Counts::default();
    let mut status = 0;
    for &name in &names {
        let counted = if name == "-" {
            count_stream(&mut *io.stdin)
        } else {
            match File::open(io.path(name)) {
                Ok(mut file) => count_stream(&mut file),
                Err(e) => {
                    status = report(io, name, &e);
                    continue;
                }
            }
        };
        match counted {
            Ok(counts) => {
                total.add(counts);
                let suffix = if named {
                    format!(" {name}")
                } else {
                    String::new()
                };
                let _ = writeln!(io.stdout, "{}{suffix}", wc_fields(counts, show));
            }
            Err(e) => status = report(io, name, &e),
        }
    }
    if names.len() > 1 {
        let _ = writeln!(io.stdout, "{} total", wc_fields(total, show));
    }
    status
}

#[cfg(test)]
mod tests;

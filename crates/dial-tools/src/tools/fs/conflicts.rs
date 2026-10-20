//! `conflict://` read target: merge-conflict hunks located by their markers.
//!
//! `conflict://` lists the files Git reports as unmerged with their hunk counts.
//! `conflict://<path>` summarizes each hunk's ours/base/theirs line ranges, then reads exactly the
//! marked ranges through the ordinary `read` path, so the result carries the usual edit anchors and
//! a hunk can be resolved with one `edit` that replaces its whole range, markers included.

use std::ops::RangeInclusive;
use std::sync::atomic::AtomicBool;

use serde_json::Value;

use crate::tools::Workspace;

/// Files listed by a bare `conflict://`.
const MAX_LISTED: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Hunk {
    /// The whole hunk, `<<<<<<<` through `>>>>>>>` (1-based lines).
    pub span: RangeInclusive<usize>,
    pub ours_label: String,
    pub ours: Option<RangeInclusive<usize>>,
    pub base: Option<Option<RangeInclusive<usize>>>,
    pub theirs_label: String,
    pub theirs: Option<RangeInclusive<usize>>,
}

/// A line made of exactly seven `ch` markers, optionally followed by a space and a label.
fn marker(line: &str, ch: u8) -> Option<&str> {
    let bytes = line.as_bytes();
    if bytes.len() < 7 || !bytes[..7].iter().all(|b| *b == ch) {
        return None;
    }
    match bytes.get(7) {
        None => Some(""),
        Some(b' ') => Some(line[8..].trim()),
        Some(_) => None,
    }
}

/// Lines `first..=last` when the section is non-empty.
fn section(first: usize, last: usize) -> Option<RangeInclusive<usize>> {
    (first <= last).then_some(first..=last)
}

/// Every complete conflict hunk in `text`; a hunk left open or a stray separator is an error
/// because a half-parsed file would hide where the markers really are.
pub(super) fn hunks(text: &str) -> Result<Vec<Hunk>, String> {
    enum State {
        Outside,
        Ours {
            start: usize,
            label: String,
        },
        Base {
            start: usize,
            label: String,
            base: usize,
        },
        Theirs {
            start: usize,
            label: String,
            base: Option<(usize, usize)>,
            sep: usize,
        },
    }
    let mut out = Vec::new();
    let mut state = State::Outside;
    for (index, line) in text.lines().enumerate() {
        let n = index + 1;
        state = match state {
            State::Outside => match marker(line, b'<') {
                Some(label) => State::Ours {
                    start: n,
                    label: label.into(),
                },
                None => State::Outside,
            },
            State::Ours { start, label } => {
                if marker(line, b'|').is_some() {
                    State::Base {
                        start,
                        label,
                        base: n,
                    }
                } else if marker(line, b'=') == Some("") {
                    State::Theirs {
                        start,
                        label,
                        base: None,
                        sep: n,
                    }
                } else if marker(line, b'<').is_some() {
                    return Err(format!(
                        "nested conflict marker at line {n} inside the hunk opened at line {start}"
                    ));
                } else {
                    State::Ours { start, label }
                }
            }
            State::Base { start, label, base } => {
                if marker(line, b'=') == Some("") {
                    State::Theirs {
                        start,
                        label,
                        base: Some((base, n)),
                        sep: n,
                    }
                } else {
                    State::Base { start, label, base }
                }
            }
            State::Theirs {
                start,
                label,
                base,
                sep,
            } => {
                if let Some(theirs_label) = marker(line, b'>') {
                    let ours_end = base.map_or(sep, |(marker, _)| marker);
                    out.push(Hunk {
                        span: start..=n,
                        ours_label: label,
                        ours: section(start + 1, ours_end - 1),
                        base: base.map(|(marker, sep)| section(marker + 1, sep - 1)),
                        theirs_label: theirs_label.into(),
                        theirs: section(sep + 1, n - 1),
                    });
                    State::Outside
                } else if marker(line, b'<').is_some() {
                    return Err(format!(
                        "nested conflict marker at line {n} inside the hunk opened at line {start}"
                    ));
                } else {
                    State::Theirs {
                        start,
                        label,
                        base,
                        sep,
                    }
                }
            }
        };
    }
    match state {
        State::Outside => Ok(out),
        State::Ours { start, .. } | State::Base { start, .. } | State::Theirs { start, .. } => Err(
            format!("conflict hunk opened at line {start} is never closed with >>>>>>>"),
        ),
    }
}

fn describe(range: &Option<RangeInclusive<usize>>) -> String {
    range
        .as_ref()
        .map_or("empty".into(), |r| format!("{}-{}", r.start(), r.end()))
}

fn side(name: &str, label: &str, range: &Option<RangeInclusive<usize>>) -> String {
    if label.is_empty() {
        format!("{name} {}", describe(range))
    } else {
        format!("{name} ({label}) {}", describe(range))
    }
}

/// The per-hunk summary printed above the anchored read.
pub(super) fn summary(target: &str, hunks: &[Hunk]) -> String {
    let mut out = format!(
        "[{target}: {} conflict hunk{}]\n",
        hunks.len(),
        if hunks.len() == 1 { "" } else { "s" }
    );
    for (index, hunk) in hunks.iter().enumerate() {
        let mut parts = vec![side("ours", &hunk.ours_label, &hunk.ours)];
        if let Some(base) = &hunk.base {
            parts.push(format!("base {}", describe(base)));
        }
        parts.push(side("theirs", &hunk.theirs_label, &hunk.theirs));
        out.push_str(&format!(
            "{}. lines {}-{}: {}\n",
            index + 1,
            hunk.span.start(),
            hunk.span.end(),
            parts.join("; ")
        ));
    }
    out.push_str(
        "Resolve a hunk by replacing its whole line range, markers included, with the merged text.\n",
    );
    out
}

fn file_text(ws: &Workspace, input: &str) -> Result<String, String> {
    let path = ws.resolve(input)?;
    crate::tools::edit::edit_transaction::read(ws, &path)?
        .map(|original| original.text)
        .ok_or_else(|| format!("{input} does not exist"))
}

fn list(ws: &Workspace) -> Result<String, String> {
    let names = crate::tools::vcs::git::run_checked(
        ws.root(),
        &["diff", "--name-only", "--diff-filter=U", "--relative", "-z"],
        false,
    )
    .map_err(|error| {
        format!("conflict:// lists unmerged Git paths ({error}); pass conflict://<path> to inspect one file")
    })?;
    let paths: Vec<&str> = names.split('\0').filter(|p| !p.is_empty()).collect();
    if paths.is_empty() {
        return Ok("No unmerged paths.".into());
    }
    let mut out = format!(
        "{} unmerged path{}:\n",
        paths.len(),
        if paths.len() == 1 { "" } else { "s" }
    );
    for path in paths.iter().take(MAX_LISTED) {
        let status = match file_text(ws, path).and_then(|text| hunks(&text)) {
            Ok(found) if found.is_empty() => {
                "no conflict markers (resolved in the file, not yet staged)".into()
            }
            Ok(found) => format!(
                "{} hunk{}",
                found.len(),
                if found.len() == 1 { "" } else { "s" }
            ),
            Err(error) => error,
        };
        out.push_str(&format!("conflict://{path}: {status}\n"));
    }
    if paths.len() > MAX_LISTED {
        out.push_str(&format!(
            "[{} more paths omitted]\n",
            paths.len() - MAX_LISTED
        ));
    }
    Ok(out.trim_end().into())
}

/// Read a `conflict://` target. Single-file results end with the anchored read of every hunk.
pub(super) fn read(
    ws: &Workspace,
    input: &str,
    args: &Value,
    cancel: &AtomicBool,
    budget: usize,
) -> Result<String, String> {
    let rest = input.strip_prefix("conflict://").unwrap_or(input);
    if rest.is_empty() {
        return list(ws);
    }
    if ws.resolve(rest)?.is_dir() {
        return Err(format!(
            "conflict://{rest} must name a file; use conflict:// to list unmerged paths"
        ));
    }
    let found = hunks(&file_text(ws, rest)?)?;
    if found.is_empty() {
        return Ok(format!("No conflict markers in {rest}."));
    }
    let head = summary(input, &found);
    let ranges: Vec<String> = found
        .iter()
        .map(|hunk| format!("{}-{}", hunk.span.start(), hunk.span.end()))
        .collect();
    let mut single = args.clone();
    single["path"] = Value::String(format!("{rest}:{}", ranges.join(",")));
    let body =
        super::files::read_tool_streaming(ws, &single, cancel, budget.saturating_sub(head.len()))?;
    Ok(format!("{head}\n{body}"))
}

#[cfg(test)]
mod tests;

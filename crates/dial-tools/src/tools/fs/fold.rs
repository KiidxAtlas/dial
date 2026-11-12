//! Folding of boilerplate in whole-file reads: a leading licence comment and long runs of import
//! lines become one note, so a whole-file read spends its budget on the code itself.
//!
//! Folding applies only to unqualified whole-file anchored reads. Folded lines are not shown and
//! so never become edit witnesses; any explicit range (`path:1-20`) shows them unchanged.

use super::select::DEFAULT_READ_LINES;
use crate::tools::Workspace;
use crate::tools::edit::{edit_source::display_path, hashline};

/// A licence header must span at least this many comment lines to be folded.
const MIN_LICENCE_LINES: usize = 6;
/// An import run must span at least this many lines to be folded.
const MIN_IMPORT_LINES: usize = 12;

/// The lines a folded whole-file read shows, and the note that names what it left out.
pub(in crate::tools) struct Folded {
    pub shown: Vec<(usize, Option<usize>)>,
    pub note: String,
}

/// Plan a folded whole-file read of `path_text`; `None` when nothing is worth folding.
pub(in crate::tools) fn plan(ws: &Workspace, path_text: &str) -> Option<Folded> {
    let abs = ws.resolve_read(path_text).ok()?;
    let rel = display_path(ws.rel(&abs));
    let text = hashline::load_text(ws, &abs, &rel).ok()?;
    let lines = hashline::split_lines(&text);
    if lines.len() > DEFAULT_READ_LINES {
        return None;
    }
    let folds = folds(&lines);
    if folds.is_empty() {
        return None;
    }
    let mut shown = Vec::new();
    let mut next = 1;
    for (first, last, _) in &folds {
        if *first > next {
            shown.push((next, Some(first - 1)));
        }
        next = last + 1;
    }
    if next <= lines.len() {
        shown.push((next, Some(lines.len())));
    }
    if shown.is_empty() {
        return None;
    }
    let parts = folds
        .iter()
        .map(|(first, last, what)| format!("{first}-{last} {what}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (first, last, _) = folds[0];
    Some(Folded {
        shown,
        note: format!(
            "[Folded lines {parts}: not shown and not edit witnesses; read them with a range, e.g. `{rel}:{first}-{last}`]"
        ),
    })
}

/// `(first, last, description)` 1-based inclusive line runs worth folding, in file order.
pub(in crate::tools) fn folds(lines: &[&str]) -> Vec<(usize, usize, &'static str)> {
    let mut out = Vec::new();
    let mut at = usize::from(lines.first().is_some_and(|line| line.starts_with("#!")));
    if let Some(end) = licence_end(lines, at) {
        out.push((at + 1, end + 1, "licence header"));
        at = end + 1;
    }
    while at < lines.len() {
        if let Some(end) = import_run_end(lines, at)
            && end + 1 - at >= MIN_IMPORT_LINES
        {
            out.push((at + 1, end + 1, "imports"));
            at = end + 1;
        } else {
            at += 1;
        }
    }
    out
}

fn is_comment(line: &str) -> bool {
    let line = line.trim_start();
    ["//", "#", "/*", "*", "--", ";", "<!--", "\"\"\"", "'''"]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        && !line.starts_with("#[")
        && !line.starts_with("#include")
        && !line.starts_with("#!")
}

/// 0-based last line of a licence comment block starting at `start`.
fn licence_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut end = start;
    while end < lines.len() && is_comment(lines[end]) {
        end += 1;
    }
    if end - start < MIN_LICENCE_LINES {
        return None;
    }
    let block = lines[start..end].join("\n").to_ascii_lowercase();
    ["license", "licence", "copyright", "spdx-license-identifier"]
        .iter()
        .any(|word| block.contains(word))
        .then_some(end - 1)
}

fn starts_import(line: &str) -> bool {
    let line = line.trim_start();
    let line = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    [
        "use ",
        "import ",
        "from ",
        "#include ",
        "#include<",
        "using ",
        "require ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
        || (line.starts_with("const ") || line.starts_with("let ") || line.starts_with("var "))
            && line.contains("= require(")
}

/// 0-based last line of an import run starting at `start`: import statements (with their
/// bracketed continuation lines) and the blank or comment lines between them.
fn import_run_end(lines: &[&str], start: usize) -> Option<usize> {
    if !starts_import(lines[start]) {
        return None;
    }
    let mut last = start;
    let mut depth: i32 = 0;
    let mut at = start;
    while at < lines.len() {
        let line = lines[at];
        if depth > 0 || starts_import(line) {
            depth += line.matches(['{', '(']).count() as i32;
            depth -= line.matches(['}', ')']).count() as i32;
            depth = depth.max(0);
            last = at;
        } else if !(line.trim().is_empty() || is_comment(line)) {
            break;
        }
        at += 1;
    }
    Some(last)
}

#[cfg(test)]
mod tests;

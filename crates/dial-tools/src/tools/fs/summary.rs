//! Structural summary of a whole-file `read` that would otherwise be cut off.
//!
//! A source file in a supported language that exceeds the read cap is returned with its long
//! function bodies elided, instead of a truncated prefix: imports, type definitions, signatures,
//! closing lines and short bodies keep their real line numbers and anchors, and each elided run is
//! one marker row naming the exact range to read. Only the rows shown are recorded as seen, so an
//! edit anchored inside an elided body is refused until that range has been read.
//!
//! Applies in hashline mode to reads without a selector, `offset` or `limit`. Files that fit, files
//! with nothing worth eliding and files without a syntax parser read exactly as before.

use super::select::{MAX_READ_LINE, READ_FOOTER_RESERVE};
use crate::tools::Workspace;
use crate::tools::edit::{edit_source, hashline};
use ast_grep_core::{AstGrep, Language};
use ast_grep_language::SupportLang;

/// Function-like nodes whose `body` may be elided; containers (impls, classes) are kept and recursed.
const ELIDABLE: &[&str] = &[
    "function_item",
    "function_definition",
    "function_declaration",
    "method_declaration",
    "method_definition",
    "arrow_function",
];
/// Smallest run of body lines worth a marker, tried from the most to the least conservative.
const MIN_ELIDED: [usize; 3] = [8, 4, 2];

/// Inclusive 1-based line run replaced by one marker row.
type Elision = (usize, usize);

/// Runs of at least `min` body lines in the outermost function-like declarations of `lf`.
fn elisions(lf: &str, lang: SupportLang, min: usize) -> Vec<Elision> {
    let Ok(tree) = AstGrep::try_new(lf, lang) else {
        return Vec::new();
    };
    let mut runs: Vec<Elision> = Vec::new();
    for node in tree.root().dfs() {
        if !ELIDABLE.contains(&node.kind().as_ref()) {
            continue;
        }
        let start = node.start_pos().line() + 1;
        // Pre-order: anything inside an already elided run is gone with it.
        if runs.last().is_some_and(|&(_, to)| start <= to) {
            continue;
        }
        let Some(body) = node.field("body") else {
            continue;
        };
        let (first, last) = (body.start_pos().line() + 1, body.end_pos().line() + 1);
        // Braced bodies keep their opening and closing lines; indented ones (Python) elide whole.
        let braced = lf.as_bytes().get(body.range().start) == Some(&b'{');
        let run = if braced {
            (first + 1, last.saturating_sub(1))
        } else {
            (first, last)
        };
        if run.1 >= run.0 && run.1 - run.0 + 1 >= min {
            runs.push(run);
        }
    }
    runs
}

/// The summarized read of `path_text`, or `None` when the ordinary read applies (the file is
/// small, unsupported, unreadable here, or no summary would fit the budget).
pub(in crate::tools) fn try_read(
    ws: &Workspace,
    path_text: &str,
    limit: usize,
    budget: usize,
) -> Option<String> {
    let abs = ws.resolve(path_text).ok()?;
    let meta = std::fs::metadata(&abs).ok()?;
    let lang = SupportLang::from_path(&abs)?;
    if !meta.is_file() || meta.len() > hashline::MAX_FILE {
        return None;
    }
    let raw = crate::tools::edit::edit_transaction::read(ws, &abs)
        .ok()??
        .text;
    if raw.contains('\0') {
        return None;
    }
    let source = edit_source::Source::load(&abs, Some(&raw)).ok()?;
    let lf = source.text.as_ref();
    let lines = hashline::split_lines(lf);
    let row_limit = budget.saturating_sub(READ_FOOTER_RESERVE);
    let rows: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let (text, cut) = hashline::truncate_chars(line, MAX_READ_LINE);
            format!(
                "{}:{text}{}\n",
                index + 1,
                if cut { " …[line truncated]" } else { "" }
            )
        })
        .collect();
    let fits = lines.len() <= limit && rows.iter().map(String::len).sum::<usize>() <= row_limit;
    if fits {
        return None;
    }
    let rel = edit_source::display_path(ws.rel(&abs));
    for min in MIN_ELIDED {
        let runs = elisions(lf, lang, min);
        if runs.is_empty() {
            continue;
        }
        let Some((body, witnessed)) = render(&rel, &lines, &rows, &runs, row_limit, limit) else {
            continue;
        };
        let elided: usize = runs.iter().map(|&(from, to)| to - from + 1).sum();
        let tag = ws.store().record_version(&abs, lf, None, &witnessed);
        return Some(format!(
            "[{rel}#{tag}]\n[Source raw SHA256:{}]\n{body}[summarized view: {} of {} lines shown, {elided} lines of {} function bodies elided; elided lines are not edit witnesses. Read an elided range (e.g. `{rel}:{}-{}`) before editing it, or `{rel}:1-` for the full file]\n",
            edit_source::revision_hex(&raw),
            lines.len() - elided,
            lines.len(),
            runs.len(),
            runs[0].0,
            runs[0].1,
        ));
    }
    None
}

/// Rows with each run replaced by its marker, and the shown lines as witness ranges. `None` when
/// the result exceeds `row_limit` bytes or `max_rows` rows (markers included).
fn render(
    rel: &str,
    lines: &[&str],
    rows: &[String],
    runs: &[Elision],
    row_limit: usize,
    max_rows: usize,
) -> Option<(String, Vec<(usize, usize)>)> {
    let mut out = String::new();
    let mut emitted = 0;
    let mut witnessed = Vec::new();
    let mut pending = runs.iter().peekable();
    let mut number = 1;
    while number <= lines.len() {
        if let Some(&&(from, to)) = pending.peek().filter(|&&&(from, _)| from == number) {
            out.push_str(&format!(
                "  ⋮ lines {from}-{to} elided (read {rel}:{from}-{to})\n"
            ));
            pending.next();
            number = to + 1;
        } else {
            out.push_str(&rows[number - 1]);
            if !rows[number - 1].ends_with(" …[line truncated]\n") {
                witnessed.push((number, number));
            }
            number += 1;
        }
        emitted += 1;
        if out.len() > row_limit || emitted > max_rows {
            return None;
        }
    }
    Some((out, witnessed))
}

#[cfg(test)]
mod tests;

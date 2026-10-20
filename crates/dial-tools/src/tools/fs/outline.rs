//! `read path:outline`: a source file's syntax declarations with 1-based line ranges.
//!
//! Declarations come from the same tree-sitter extraction as `find` and the codebase map
//! ([`navigation::declarations`]). Rows are structural summaries, not verbatim source rows, so
//! they never record an edit witness: the model must read a line range before editing it.

use crate::tools::code::navigation;
use crate::tools::edit::{edit_source::display_path, hashline};
use crate::tools::{Workspace, io_message};
use ast_grep_core::Language;
use ast_grep_language::SupportLang;
use std::path::Path;

pub(in crate::tools) const SELECTOR: &str = ":outline";
const MAX_ROWS: usize = 400;
const MAX_SIGNATURE: usize = 120;

/// The plain path of an outline request, unless a file literally named `…:outline` exists.
pub(in crate::tools) fn target<'a>(ws: &Workspace, input: &'a str) -> Option<&'a str> {
    let path = input.strip_suffix(SELECTOR).filter(|p| !p.is_empty())?;
    (!ws.resolve(input).is_ok_and(|literal| literal.exists())).then_some(path)
}

/// `rust:function_item` → `function`.
fn kind_label(kind: &str) -> &str {
    let kind = kind.split_once(':').map_or(kind, |(_, kind)| kind);
    ["_item", "_definition", "_declaration"]
        .iter()
        .find_map(|suffix| kind.strip_suffix(suffix))
        .unwrap_or(kind)
}

pub(in crate::tools) fn read(ws: &Workspace, input: &str, budget: usize) -> Result<String, String> {
    let path = ws.resolve(input)?;
    let meta = std::fs::metadata(&path).map_err(|e| io_message("reading", input, &e))?;
    if meta.is_dir() {
        return Err(format!(
            "{input:?} is a directory; `:outline` needs a source file"
        ));
    }
    let rel = display_path(ws.rel(&path));
    if SupportLang::from_path(&path).is_none() {
        return Err(format!(
            "no outline for {rel:?}: no syntax parser for this file type; read line ranges instead (e.g. `{rel}:1-200`)"
        ));
    }
    let text = hashline::load_text(ws, &path, &rel)?;
    let lines: Vec<&str> = text.lines().collect();
    let declarations = navigation::declarations(Path::new(&path), &text);
    if declarations.is_empty() {
        return Ok(format!(
            "{rel} outline: no declarations found ({} lines); read line ranges instead (e.g. `{rel}:1-200`)",
            lines.len()
        ));
    }
    let mut out = format!(
        "{rel} outline ({} declarations, {} lines; `start-end kind first line`; summaries only, not edit witnesses: read a range before editing):\n",
        declarations.len(),
        lines.len()
    );
    let row_limit = budget.saturating_sub(super::select::READ_FOOTER_RESERVE);
    // Enclosing declarations' end lines; declarations arrive in pre-order (parents first).
    let mut open: Vec<usize> = Vec::new();
    let mut shown = 0;
    for (start, end, kind) in &declarations {
        while open.last().is_some_and(|parent_end| parent_end < end) {
            open.pop();
        }
        let first = lines.get(start - 1).map_or("", |line| line.trim());
        let mut signature: String = first.chars().take(MAX_SIGNATURE).collect();
        if signature.len() < first.len() {
            signature.push('…');
        }
        let row = format!(
            "{}{start}-{end} {} {signature}\n",
            "  ".repeat(open.len()),
            kind_label(kind)
        );
        if shown == MAX_ROWS || out.len() + row.len() > row_limit {
            break;
        }
        out.push_str(&row);
        shown += 1;
        open.push(*end);
    }
    if shown < declarations.len() {
        out.push_str(&format!(
            "[{} more declarations omitted; read line ranges for the rest]\n",
            declarations.len() - shown
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;

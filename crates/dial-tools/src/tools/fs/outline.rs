//! `read path:outline`: a source file's syntax declarations with 1-based line ranges.
//!
//! Declarations come from the same tree-sitter extraction as `find` and the codebase map
//! ([`navigation::declarations`]). Rows are structural summaries, not verbatim source rows, so
//! they never record an edit witness: the model must read a line range before editing it.
//!
//! The same declarations answer three related requests:
//! - `dir:outline` lists the top-level declarations of every source file under a directory;
//! - `path:outline=NAME` returns the outline followed by the named declarations as anchored rows;
//! - `path:sym=NAME` ([`resolve_symbols`]) reads just the named declarations, with the doc
//!   comments and attributes directly above them.

use super::select::{READ_FOOTER_RESERVE, Selector};
use crate::tools::code::navigation;
use crate::tools::edit::{edit_source::display_path, hashline};
use crate::tools::{Workspace, io_message};
use ast_grep_core::Language;
use ast_grep_language::SupportLang;
use std::path::Path;

pub(in crate::tools) const SELECTOR: &str = ":outline";
/// `path:outline=NAME[,NAME]`: the outline, then those declarations in full.
const EXPAND_SELECTOR: &str = ":outline=";
const MAX_ROWS: usize = 400;
const MAX_SIGNATURE: usize = 120;
/// Source files a directory outline visits, and top-level declarations it lists per file.
const MAX_DIR_FILES: usize = 300;
const MAX_DIR_DECLARATIONS: usize = 40;
/// Larger files are listed by name only in a directory outline.
const MAX_DIR_FILE_BYTES: u64 = 512 * 1024;
/// Several declarations sharing a name are all returned only while they stay this small.
const MAX_AMBIGUOUS: usize = 3;
const MAX_AMBIGUOUS_LINES: usize = 400;

/// An outline request: the plain path and the declarations to expand after it.
pub(in crate::tools) struct Outline<'a> {
    pub path: &'a str,
    pub expand: Vec<String>,
}

/// The outline request in `input`, unless a file literally named like the selector exists.
pub(in crate::tools) fn target<'a>(ws: &Workspace, input: &'a str) -> Option<Outline<'a>> {
    if ws.resolve_read(input).is_ok_and(|literal| literal.exists()) {
        return None;
    }
    if let Some(path) = input.strip_suffix(SELECTOR).filter(|p| !p.is_empty()) {
        return Some(Outline {
            path,
            expand: Vec::new(),
        });
    }
    let at = input.rfind(EXPAND_SELECTOR)?;
    let path = &input[..at];
    let expand: Vec<String> = input[at + EXPAND_SELECTOR.len()..]
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    (!path.is_empty() && !expand.is_empty()).then_some(Outline { path, expand })
}

/// The path an outline request names, for error suggestions.
pub(in crate::tools) fn plain_path(input: &str) -> &str {
    input.rfind(EXPAND_SELECTOR).map_or_else(
        || input.strip_suffix(SELECTOR).unwrap_or(input),
        |at| &input[..at],
    )
}

/// `rust:function_item` → `function`.
fn kind_label(kind: &str) -> &str {
    let kind = kind.split_once(':').map_or(kind, |(_, kind)| kind);
    ["_item", "_definition", "_declaration"]
        .iter()
        .find_map(|suffix| kind.strip_suffix(suffix))
        .unwrap_or(kind)
}

/// The first line of a declaration, trimmed and cut to `max` characters.
fn signature(lines: &[&str], start: usize, max: usize) -> String {
    let first = lines.get(start - 1).map_or("", |line| line.trim());
    let mut signature: String = first.chars().take(max).collect();
    if signature.len() < first.len() {
        signature.push('…');
    }
    signature
}

pub(in crate::tools) fn read(
    ws: &Workspace,
    request: &Outline<'_>,
    budget: usize,
) -> Result<String, String> {
    let input = request.path;
    let path = ws.resolve_read(input)?;
    let meta = std::fs::metadata(&path).map_err(|e| io_message("reading", input, &e))?;
    if meta.is_dir() {
        if !request.expand.is_empty() {
            return Err(format!(
                "{input:?} is a directory; `:outline=NAME` needs a source file"
            ));
        }
        return read_directory(ws, &path, budget);
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
    // An expanded outline keeps half the budget for the declarations it expands.
    let outline_budget = if request.expand.is_empty() {
        budget
    } else {
        budget / 2
    };
    let mut out = format!(
        "{rel} outline ({} declarations, {} lines; `start-end kind first line`; summaries only, not edit witnesses: read a range before editing):\n",
        declarations.len(),
        lines.len()
    );
    let row_limit = outline_budget.saturating_sub(READ_FOOTER_RESERVE);
    // Enclosing declarations' end lines; declarations arrive in pre-order (parents first).
    let mut open: Vec<usize> = Vec::new();
    let mut shown = 0;
    for (start, end, kind) in &declarations {
        while open.last().is_some_and(|parent_end| parent_end < end) {
            open.pop();
        }
        let row = format!(
            "{}{start}-{end} {} {}\n",
            "  ".repeat(open.len()),
            kind_label(kind),
            signature(&lines, *start, MAX_SIGNATURE)
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
    if request.expand.is_empty() {
        return Ok(out);
    }
    let mut selector = Selector {
        symbols: request.expand.clone(),
        ..Selector::default()
    };
    resolve_in(&rel, &text, &declarations, &mut selector)?;
    let body = hashline::read(
        ws,
        input,
        &selector,
        1,
        super::select::MAX_READ_LINES,
        budget.saturating_sub(out.len()),
    )?;
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

/// Top-level declarations of every source file under `dir`, one indented block per file.
fn read_directory(ws: &Workspace, dir: &Path, budget: usize) -> Result<String, String> {
    let rel_dir = display_path(ws.rel(dir));
    let row_limit = budget.saturating_sub(READ_FOOTER_RESERVE);
    let mut body = String::new();
    let (mut files, mut without_parser, mut omitted) = (0usize, 0usize, 0usize);
    let mut full = false;
    let mut walk = super::walk::Walk::new(ws);
    walk.run(dir, &mut |path| {
        if SupportLang::from_path(path).is_none() {
            without_parser += 1;
            return true;
        }
        if full || files == MAX_DIR_FILES {
            omitted += 1;
            return true;
        }
        let rel = display_path(ws.rel(path));
        let block = file_block(ws, path, &rel);
        if body.len() + block.len() > row_limit {
            full = true;
            omitted += 1;
            return true;
        }
        body.push_str(&block);
        files += 1;
        true
    });
    if files == 0 && omitted == 0 {
        return Ok(format!(
            "{rel_dir} outline: no source files with a syntax parser ({without_parser} other files); list it with `read {rel_dir}`"
        ));
    }
    let mut out = format!(
        "{rel_dir} outline ({files} source files; top-level `start-end kind first line` per file; summaries only, not edit witnesses: read a file or `path:sym=NAME` before editing):\n{body}"
    );
    let mut notes = Vec::new();
    if omitted > 0 {
        notes.push(format!(
            "{omitted} more source files omitted at the output budget; outline a subdirectory"
        ));
    }
    if without_parser > 0 {
        notes.push(format!(
            "{without_parser} files without a syntax parser not listed"
        ));
    }
    if walk.truncated {
        notes.push("the directory walk stopped early at its entry limit".to_owned());
    }
    if !notes.is_empty() {
        out.push_str(&format!("[{}]\n", notes.join("; ")));
    }
    Ok(out)
}

/// One file's block of a directory outline: its line count and top-level declarations.
fn file_block(ws: &Workspace, path: &Path, rel: &str) -> String {
    let too_large = std::fs::metadata(path).map_or(true, |meta| meta.len() > MAX_DIR_FILE_BYTES);
    let text = if too_large {
        None
    } else {
        hashline::load_text(ws, path, rel).ok()
    };
    let Some(text) = text else {
        return format!("{rel} (not outlined: unreadable or larger than 512 KiB)\n");
    };
    let lines: Vec<&str> = text.lines().collect();
    let declarations = navigation::declarations(path, &text);
    let mut block = format!("{rel} ({} lines)\n", lines.len());
    let mut outer_end = 0;
    let mut listed = 0;
    let mut top = 0;
    for (start, end, kind) in &declarations {
        if *start <= outer_end {
            continue;
        }
        outer_end = *end;
        top += 1;
        if listed == MAX_DIR_DECLARATIONS {
            continue;
        }
        block.push_str(&format!(
            "  {start}-{end} {} {}\n",
            kind_label(kind),
            signature(&lines, *start, 80)
        ));
        listed += 1;
    }
    if top > listed {
        block.push_str(&format!(
            "  [{} more; `read {rel}:outline`]\n",
            top - listed
        ));
    }
    block
}

/// Turn `sel.symbols` into line ranges of `path_text`. A no-op without symbols.
pub(in crate::tools) fn resolve_symbols(
    ws: &Workspace,
    path_text: &str,
    sel: &mut Selector,
) -> Result<(), String> {
    if sel.symbols.is_empty() {
        return Ok(());
    }
    if sel.tail.is_some() || !sel.ranges.is_empty() {
        return Err("`:sym=` cannot be combined with a line range".into());
    }
    let path = ws.resolve_read(path_text)?;
    let rel = display_path(ws.rel(&path));
    if SupportLang::from_path(&path).is_none() {
        return Err(format!(
            "`:sym=` needs a source file with a syntax parser; {rel:?} has none. Use `grep` for the name, then a line range"
        ));
    }
    let text = hashline::load_text(ws, &path, &rel)?;
    let declarations = navigation::declarations(Path::new(&path), &text);
    resolve_in(&rel, &text, &declarations, sel)
}

fn resolve_in(
    rel: &str,
    text: &str,
    declarations: &[(usize, usize, String)],
    sel: &mut Selector,
) -> Result<(), String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for name in &sel.symbols {
        let found = find_symbol(&lines, declarations, name);
        let chosen: Vec<usize> = match found.strong.len() {
            1 => found.strong,
            0 if found.weak.len() == 1 => found.weak,
            n if n > 1
                && n <= MAX_AMBIGUOUS
                && found
                    .strong
                    .iter()
                    .map(|&i| declarations[i].1 - declarations[i].0 + 1)
                    .sum::<usize>()
                    <= MAX_AMBIGUOUS_LINES =>
            {
                found.strong
            }
            0 if found.weak.is_empty() => {
                return Err(format!(
                    "no declaration named {name:?} in {rel}; see `{rel}:outline`, or `grep` the name"
                ));
            }
            _ => {
                let candidates = if found.strong.is_empty() {
                    &found.weak
                } else {
                    &found.strong
                };
                let listed: Vec<String> = candidates
                    .iter()
                    .take(10)
                    .map(|&i| {
                        let (start, end, kind) = &declarations[i];
                        format!(
                            "{start}-{end} {} {}",
                            kind_label(kind),
                            signature(&lines, *start, 80)
                        )
                    })
                    .collect();
                return Err(format!(
                    "{} declarations in {rel} match {name:?}; qualify it (`Parent.{name}`) or read one range:\n{}",
                    candidates.len(),
                    listed.join("\n")
                ));
            }
        };
        for index in chosen {
            let (start, end, _) = &declarations[index];
            ranges.push((leading_context(&lines, *start), *end));
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, Option<usize>)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some((_, Some(last))) if start <= last.saturating_add(1) => *last = (*last).max(end),
            _ => merged.push((start, Some(end))),
        }
    }
    sel.ranges = merged;
    Ok(())
}

/// Declarations matching one requested name: `strong` where the name follows a declaration
/// keyword (or starts a call-like signature), `weak` where it only appears as a word.
#[derive(Default)]
struct Found {
    strong: Vec<usize>,
    weak: Vec<usize>,
}

/// Keywords that introduce the declared name in common languages.
const DECLARATION_KEYWORDS: &[&str] = &[
    "fn",
    "struct",
    "enum",
    "trait",
    "impl",
    "type",
    "mod",
    "const",
    "static",
    "union",
    "macro_rules!",
    "class",
    "def",
    "function",
    "interface",
    "func",
    "var",
    "let",
    "val",
    "object",
    "module",
    "record",
    "namespace",
    "protocol",
    "extension",
    "typealias",
];

fn find_symbol(lines: &[&str], declarations: &[(usize, usize, String)], name: &str) -> Found {
    let (parents, ident) = match name.rsplit_once(['.', ':']) {
        Some((parents, ident)) => (
            parents
                .split(['.', ':'])
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>(),
            ident,
        ),
        None => (Vec::new(), name),
    };
    let mut found = Found::default();
    for (index, (start, end, _)) in declarations.iter().enumerate() {
        let line = lines.get(start - 1).copied().unwrap_or("");
        let Some(strength) = mentions(line, ident) else {
            continue;
        };
        // Every qualifier must name an enclosing declaration, innermost last.
        let enclosing = declarations
            .iter()
            .filter(|(s, e, _)| s <= start && e >= end && (s, e) != (start, end))
            .map(|(s, _, _)| lines.get(s - 1).copied().unwrap_or(""))
            .collect::<Vec<_>>();
        if !parents.iter().all(|parent| {
            enclosing
                .iter()
                .any(|line| mentions(line, parent).is_some())
        }) {
            continue;
        }
        if strength {
            found.strong.push(index);
        } else {
            found.weak.push(index);
        }
    }
    // A match nested in another match of the same name adds nothing.
    let outermost = |list: &mut Vec<usize>| {
        let all = list.clone();
        list.retain(|&i| {
            !all.iter().any(|&j| {
                j != i
                    && declarations[j].0 <= declarations[i].0
                    && declarations[j].1 >= declarations[i].1
            })
        });
    };
    outermost(&mut found.strong);
    outermost(&mut found.weak);
    found
}

/// `Some(true)` when `line` declares `ident` (after a declaration keyword, or as the first word
/// of a call-like signature), `Some(false)` when it only mentions the word, `None` otherwise.
fn mentions(line: &str, ident: &str) -> Option<bool> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut weak = false;
    for (at, _) in line.match_indices(ident) {
        let before = line[..at].chars().next_back();
        let after = line[at + ident.len()..].chars().next();
        if before.is_some_and(is_word) || after.is_some_and(is_word) {
            continue;
        }
        let previous = line[..at]
            .trim_end()
            .rsplit(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");
        let previous = previous.trim_start_matches(|c: char| !is_word(c));
        if DECLARATION_KEYWORDS.contains(&previous) {
            return Some(true);
        }
        // `name(…)`/`name<…>(…)` as the first word after modifiers: a method or function.
        let head = line[..at].trim();
        let modifiers_only = head.split_whitespace().all(|word| {
            matches!(
                word,
                "pub"
                    | "pub(crate)"
                    | "pub(super)"
                    | "async"
                    | "static"
                    | "export"
                    | "default"
                    | "public"
                    | "private"
                    | "protected"
                    | "final"
                    | "abstract"
                    | "override"
                    | "unsafe"
                    | "extern"
                    | "const"
                    | "inline"
                    | "virtual"
                    | "get"
                    | "set"
            ) || word.starts_with("pub(")
        });
        if modifiers_only && after.is_some_and(|c| c == '(' || c == '<') {
            return Some(true);
        }
        weak = true;
    }
    weak.then_some(false)
}

/// First line of the doc comments and attributes directly above `start` (no blank line between).
fn leading_context(lines: &[&str], start: usize) -> usize {
    let mut first = start;
    while first > 1 {
        let line = lines[first - 2].trim_start();
        let attached = ["///", "//", "#[", "@", "/**", "/*", "*", "--", "\"\"\""]
            .iter()
            .any(|prefix| line.starts_with(prefix))
            && !line.starts_with("//!")
            && !line.starts_with("#!");
        if !attached {
            break;
        }
        first -= 1;
    }
    first
}

#[cfg(test)]
mod tests;

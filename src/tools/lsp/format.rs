//! Turn language-server results into compact text for the model.
//!
//! Locations print as `path:line:col  text-of-line` with 1-based line and character column.
//! Line text is read from disk, and only for files inside the workspace (checked through
//! [`Workspace::resolve`], so a server cannot make dial read elsewhere). Every list is capped.

use super::text::{char_index, path_from_uri, read_source, split_lines};
use crate::tools::Workspace;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_LOCATIONS: usize = 50;
const MAX_SYMBOLS: usize = 300;
const MAX_WORKSPACE_SYMBOLS: usize = 50;
const MAX_DIAGNOSTICS: usize = 50;
const MAX_EDITS: usize = 100;
const MAX_LINE_TEXT: usize = 160;
const MAX_MESSAGE: usize = 300;
const MAX_NEW_TEXT: usize = 200;
const MAX_HOVER: usize = 4000;

/// Cut `text` to at most `limit` characters, marking the cut.
pub fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Lazily read source lines of workspace files, for line text and column conversion.
pub struct Lines<'a> {
    ws: &'a Workspace,
    files: HashMap<PathBuf, Option<Vec<String>>>,
}

impl<'a> Lines<'a> {
    pub fn new(ws: &'a Workspace) -> Self {
        Self {
            ws,
            files: HashMap::new(),
        }
    }

    fn file(&mut self, path: &Path) -> Option<&[String]> {
        let ws = self.ws;
        self.files
            .entry(path.to_owned())
            .or_insert_with(|| {
                let resolved = ws.resolve(&path.to_string_lossy()).ok()?;
                read_source(&resolved).ok().map(|text| split_lines(&text))
            })
            .as_deref()
    }

    fn text(&mut self, path: &Path, line: u32) -> Option<String> {
        self.file(path)?
            .get(line as usize)
            .map(|l| clip(l.trim(), MAX_LINE_TEXT))
    }

    /// 1-based character column for a UTF-16 offset; without the file the offset is all we have.
    fn column(&mut self, path: &Path, line: u32, utf16: u32) -> usize {
        match self.file(path).and_then(|lines| lines.get(line as usize)) {
            Some(content) => char_index(content, utf16) + 1,
            None => utf16 as usize + 1,
        }
    }

    fn display(&self, path: &Path) -> String {
        self.ws.rel(path)
    }

    /// `path:line:col` for a position.
    fn place(&mut self, path: &Path, line: u32, utf16: u32) -> String {
        let column = self.column(path, line, utf16);
        format!("{}:{}:{column}", self.display(path), line.saturating_add(1))
    }

    /// `path:line:col  text-of-line`.
    fn describe(&mut self, path: &Path, line: u32, utf16: u32) -> String {
        let place = self.place(path, line, utf16);
        match self.text(path, line) {
            Some(text) => format!("{place}  {text}"),
            None => place,
        }
    }
}

struct Spot {
    path: PathBuf,
    line: u32,
    character: u32,
}

fn position(value: &Value) -> Option<(u32, u32)> {
    let line = u32::try_from(value.get("line")?.as_u64()?).unwrap_or(u32::MAX);
    let character = u32::try_from(value.get("character")?.as_u64()?).unwrap_or(u32::MAX);
    Some((line, character))
}

fn spot(uri: &Value, range: Option<&Value>) -> Option<Spot> {
    let path = path_from_uri(uri.as_str()?)?;
    let (line, character) = range
        .and_then(|r| position(r.get("start")?))
        .unwrap_or((0, 0));
    Some(Spot {
        path,
        line,
        character,
    })
}

/// `Location`, `LocationLink`, or arrays of either (as returned by definition-like requests).
fn spots(result: &Value) -> Vec<Spot> {
    let items: &[Value] = match result {
        Value::Array(items) => items,
        Value::Object(_) => std::slice::from_ref(result),
        _ => &[],
    };
    let mut found: Vec<Spot> = items
        .iter()
        .filter_map(|item| match item.get("targetUri") {
            Some(uri) => spot(
                uri,
                item.get("targetSelectionRange")
                    .or_else(|| item.get("targetRange")),
            ),
            None => spot(item.get("uri")?, item.get("range")),
        })
        .collect();
    let mut seen = HashSet::new();
    found.retain(|s| seen.insert((s.path.clone(), s.line, s.character)));
    found
}

pub fn locations(lines: &mut Lines, result: &Value, noun: &str) -> String {
    let found = spots(result);
    if found.is_empty() {
        return format!("no {noun} found");
    }
    let mut out = vec![format!(
        "{} {noun}{}",
        found.len(),
        if found.len() == 1 { "" } else { "s" }
    )];
    out.extend(
        found
            .iter()
            .take(MAX_LOCATIONS)
            .map(|s| lines.describe(&s.path, s.line, s.character)),
    );
    if found.len() > MAX_LOCATIONS {
        out.push(format!("… {} more not shown", found.len() - MAX_LOCATIONS));
    }
    out.join("\n")
}

/// `MarkupContent`, `MarkedString`, or an array of them, flattened to plain text.
fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(hover_text)
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => map
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

pub fn hover(result: &Value) -> String {
    let text = hover_text(result.get("contents").unwrap_or(&Value::Null));
    let text = text.trim();
    if text.is_empty() {
        return "no hover information".to_owned();
    }
    clip(text, MAX_HOVER)
}

const SYMBOL_KINDS: [&str; 26] = [
    "file",
    "module",
    "namespace",
    "package",
    "class",
    "method",
    "property",
    "field",
    "constructor",
    "enum",
    "interface",
    "function",
    "variable",
    "constant",
    "string",
    "number",
    "boolean",
    "array",
    "object",
    "key",
    "null",
    "enum-member",
    "struct",
    "event",
    "operator",
    "type-parameter",
];

fn kind_name(symbol: &Value) -> &'static str {
    let kind = symbol.get("kind").and_then(Value::as_u64).unwrap_or(0) as usize;
    kind.checked_sub(1)
        .and_then(|i| SYMBOL_KINDS.get(i))
        .copied()
        .unwrap_or("symbol")
}

fn symbol_name(symbol: &Value) -> &str {
    symbol.get("name").and_then(Value::as_str).unwrap_or("?")
}

fn push_outline(
    lines: &mut Lines,
    path: &Path,
    symbols: &[Value],
    depth: usize,
    out: &mut Vec<String>,
    total: &mut usize,
) {
    for symbol in symbols {
        *total += 1;
        if out.len() < MAX_SYMBOLS {
            // DocumentSymbol has `selectionRange`; the flat SymbolInformation has `location`.
            let range = symbol
                .get("selectionRange")
                .or_else(|| symbol.get("range"))
                .or_else(|| symbol.pointer("/location/range"));
            let (line, character) = range
                .and_then(|r| position(r.get("start")?))
                .unwrap_or((0, 0));
            let column = lines.column(path, line, character);
            out.push(format!(
                "{}:{column}  {}{} {}",
                line.saturating_add(1),
                "  ".repeat(depth),
                kind_name(symbol),
                symbol_name(symbol)
            ));
        }
        if let Some(children) = symbol.get("children").and_then(Value::as_array) {
            push_outline(lines, path, children, depth + 1, out, total);
        }
    }
}

pub fn document_symbols(lines: &mut Lines, path: &Path, result: &Value) -> String {
    let Some(symbols) = result.as_array().filter(|s| !s.is_empty()) else {
        return "no symbols".to_owned();
    };
    let mut out = Vec::new();
    let mut total = 0;
    push_outline(lines, path, symbols, 0, &mut out, &mut total);
    if total > out.len() {
        out.push(format!("… {} more not shown", total - out.len()));
    }
    format!(
        "{} symbols in {}\n{}",
        total,
        lines.display(path),
        out.join("\n")
    )
}

pub fn workspace_symbols(lines: &mut Lines, result: &Value) -> String {
    let Some(symbols) = result.as_array().filter(|s| !s.is_empty()) else {
        return "no matching symbols".to_owned();
    };
    let mut out = vec![format!("{} symbols", symbols.len())];
    for symbol in symbols.iter().take(MAX_WORKSPACE_SYMBOLS) {
        let container = symbol
            .get("containerName")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty());
        let label = match container {
            Some(container) => format!(
                "{} {} ({container})",
                kind_name(symbol),
                symbol_name(symbol)
            ),
            None => format!("{} {}", kind_name(symbol), symbol_name(symbol)),
        };
        let location = symbol.get("location").unwrap_or(&Value::Null);
        let place = match spot(
            location.get("uri").unwrap_or(&Value::Null),
            location.get("range"),
        ) {
            // A WorkspaceSymbol may omit the range; then only the file is known.
            Some(s) if location.get("range").is_none() => lines.display(&s.path),
            Some(s) => lines.place(&s.path, s.line, s.character),
            None => "?".to_owned(),
        };
        out.push(format!("{place}  {label}"));
    }
    if symbols.len() > MAX_WORKSPACE_SYMBOLS {
        out.push(format!(
            "… {} more not shown",
            symbols.len() - MAX_WORKSPACE_SYMBOLS
        ));
    }
    out.join("\n")
}

pub fn diagnostics(lines: &mut Lines, path: &Path, items: &[Value]) -> String {
    if items.is_empty() {
        return format!("no diagnostics for {}", lines.display(path));
    }
    let mut sorted: Vec<&Value> = items.iter().collect();
    sorted.sort_by_key(|d| {
        d.pointer("/range/start")
            .and_then(position)
            .unwrap_or((0, 0))
    });
    let count = |severity: u64| {
        items
            .iter()
            .filter(|d| d.get("severity").and_then(Value::as_u64).unwrap_or(1) == severity)
            .count()
    };
    let mut out = vec![format!(
        "{} diagnostics in {} ({} errors, {} warnings)",
        items.len(),
        lines.display(path),
        count(1),
        count(2)
    )];
    for item in sorted.iter().take(MAX_DIAGNOSTICS) {
        let (line, character) = item
            .pointer("/range/start")
            .and_then(position)
            .unwrap_or((0, 0));
        let severity = match item.get("severity").and_then(Value::as_u64) {
            Some(2) => "warning",
            Some(3) => "info",
            Some(4) => "hint",
            _ => "error",
        };
        let source = item
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let code = match item.get("code") {
            Some(Value::String(code)) => code.clone(),
            Some(Value::Number(code)) => code.to_string(),
            _ => String::new(),
        };
        let tag = [source, code.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        let tag = if tag.is_empty() {
            String::new()
        } else {
            format!("[{tag}]")
        };
        let message = item
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = clip(&message.trim().replace('\n', " / "), MAX_MESSAGE);
        out.push(format!(
            "{}  {severity}{tag} {message}",
            lines.place(path, line, character)
        ));
    }
    if items.len() > MAX_DIAGNOSTICS {
        out.push(format!(
            "… {} more not shown",
            items.len() - MAX_DIAGNOSTICS
        ));
    }
    out.join("\n")
}

struct TextEdit {
    start: (u32, u32),
    end: (u32, u32),
    new_text: String,
}

fn text_edit(edit: &Value) -> Option<TextEdit> {
    Some(TextEdit {
        start: position(edit.pointer("/range/start")?)?,
        end: position(edit.pointer("/range/end")?)?,
        new_text: edit.get("newText")?.as_str()?.to_owned(),
    })
}

/// Everything a `WorkspaceEdit` would do, grouped by file; the resource operations
/// (`create`/`rename`/`delete`) are kept as plain descriptions.
fn collect_edits(edit: &Value, lines: &Lines) -> (Vec<(PathBuf, Vec<TextEdit>)>, Vec<String>) {
    let mut files: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
    let mut operations = Vec::new();
    let mut add =
        |path: PathBuf, edits: Vec<TextEdit>| match files.iter_mut().find(|(p, _)| *p == path) {
            Some((_, existing)) => existing.extend(edits),
            None => files.push((path, edits)),
        };
    if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            if let Some(path) = path_from_uri(uri) {
                add(
                    path,
                    edits
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(text_edit)
                        .collect(),
                );
            }
        }
    }
    for change in edit
        .get("documentChanges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let shown = |key: &str| {
            change
                .get(key)
                .and_then(Value::as_str)
                .and_then(path_from_uri)
                .map(|p| lines.display(&p))
        };
        match change.get("kind").and_then(Value::as_str) {
            Some("create") => {
                operations.push(format!("create {}", shown("uri").unwrap_or_default()))
            }
            Some("delete") => {
                operations.push(format!("delete {}", shown("uri").unwrap_or_default()))
            }
            Some("rename") => operations.push(format!(
                "rename {} -> {}",
                shown("oldUri").unwrap_or_default(),
                shown("newUri").unwrap_or_default()
            )),
            _ => {
                let Some(path) = change
                    .pointer("/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(path_from_uri)
                else {
                    continue;
                };
                add(
                    path,
                    change
                        .get("edits")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(text_edit)
                        .collect(),
                );
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    (files, operations)
}

/// Render a `WorkspaceEdit` as a reviewable list. Nothing is applied.
pub fn workspace_edit(lines: &mut Lines, result: &Value) -> String {
    let (mut files, operations) = collect_edits(result, lines);
    let total: usize = files.iter().map(|(_, edits)| edits.len()).sum();
    if total == 0 && operations.is_empty() {
        return "the server proposes no changes".to_owned();
    }
    let mut out = vec![format!(
        "rename preview, NOT applied: {total} edits in {} files. Apply them with the edit tool.",
        files.len()
    )];
    out.extend(operations);
    let mut shown = 0;
    for (path, edits) in &mut files {
        edits.sort_by_key(|e| e.start);
        for edit in edits.iter() {
            if shown == MAX_EDITS {
                break;
            }
            shown += 1;
            let from = lines.place(path, edit.start.0, edit.start.1);
            let end = format!(
                "{}:{}",
                edit.end.0.saturating_add(1),
                lines.column(path, edit.end.0, edit.end.1)
            );
            let was = lines
                .text(path, edit.start.0)
                .map(|t| format!("  | {t}"))
                .unwrap_or_default();
            let new_text =
                serde_json::to_string(&clip(&edit.new_text, MAX_NEW_TEXT)).unwrap_or_default();
            out.push(format!("{from}-{end} -> {new_text}{was}"));
        }
    }
    if total > shown {
        out.push(format!("… {} more edits not shown", total - shown));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn project() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/a.rs"),
            "fn alpha() {}\n    let s = \"😀\"; beta();\n",
        )
        .unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    fn uri(ws: &Workspace, rel: &str) -> String {
        super::super::text::uri_from_path(&ws.root().join(rel))
    }

    fn at(line: u32, character: u32) -> Value {
        json!({"start": {"line": line, "character": character}, "end": {"line": line, "character": character + 1}})
    }

    #[test]
    fn locations_show_path_line_col_and_line_text() {
        let (_dir, ws) = project();
        // `beta` starts at UTF-16 offset 18 (the emoji takes two units), i.e. character column 18.
        let result = json!([{"uri": uri(&ws, "src/a.rs"), "range": at(1, 18)}]);
        let text = locations(&mut Lines::new(&ws), &result, "reference");
        assert_eq!(text, "1 reference\nsrc/a.rs:2:18  let s = \"😀\"; beta();");
    }

    #[test]
    fn location_links_and_single_locations_are_understood() {
        let (_dir, ws) = project();
        let link = json!([{"targetUri": uri(&ws, "src/a.rs"), "targetRange": at(0, 0), "targetSelectionRange": at(0, 3)}]);
        assert_eq!(
            locations(&mut Lines::new(&ws), &link, "definition"),
            "1 definition\nsrc/a.rs:1:4  fn alpha() {}"
        );
        let single = json!({"uri": uri(&ws, "src/a.rs"), "range": at(0, 0)});
        assert!(locations(&mut Lines::new(&ws), &single, "definition").contains("src/a.rs:1:1"));
        assert_eq!(
            locations(&mut Lines::new(&ws), &Value::Null, "definition"),
            "no definition found"
        );
        assert_eq!(
            locations(&mut Lines::new(&ws), &json!([]), "reference"),
            "no reference found"
        );
    }

    #[test]
    fn locations_outside_the_workspace_show_only_the_path() {
        let (_dir, ws) = project();
        let result = json!([{"uri": "file:///etc/hosts", "range": at(0, 0)}]);
        let text = locations(&mut Lines::new(&ws), &result, "definition");
        assert_eq!(
            text, "1 definition\n/etc/hosts:1:1",
            "line text must not be read outside the workspace"
        );
    }

    #[test]
    fn long_location_lists_are_capped_with_a_marker() {
        let (_dir, ws) = project();
        let items: Vec<Value> = (0..80)
            .map(|i| json!({"uri": uri(&ws, "src/a.rs"), "range": at(i % 2, i)}))
            .collect();
        let text = locations(&mut Lines::new(&ws), &Value::Array(items), "reference");
        assert_eq!(text.lines().count(), 1 + MAX_LOCATIONS + 1);
        assert!(
            text.starts_with("80 references") && text.ends_with("… 30 more not shown"),
            "{text}"
        );
    }

    #[test]
    fn hover_flattens_every_contents_shape() {
        assert_eq!(
            hover(&json!({"contents": {"kind": "markdown", "value": "```rust\nfn a()\n```"}})),
            "```rust\nfn a()\n```"
        );
        assert_eq!(hover(&json!({"contents": "plain"})), "plain");
        assert_eq!(
            hover(&json!({"contents": ["one", {"language": "rust", "value": "two"}]})),
            "one\n\ntwo"
        );
        assert_eq!(hover(&Value::Null), "no hover information");
        assert_eq!(
            hover(&json!({"contents": {"kind": "markdown", "value": "  "}})),
            "no hover information"
        );
        assert!(hover(&json!({"contents": "x".repeat(MAX_HOVER * 2)})).ends_with('…'));
    }

    #[test]
    fn document_symbols_nest_hierarchical_results() {
        let (_dir, ws) = project();
        let result = json!([{
            "name": "Outer", "kind": 23, "range": at(0, 0), "selectionRange": at(0, 3),
            "children": [{"name": "inner", "kind": 6, "range": at(1, 4), "selectionRange": at(1, 4)}],
        }]);
        let path = ws.root().join("src/a.rs");
        let text = document_symbols(&mut Lines::new(&ws), &path, &result);
        assert_eq!(
            text,
            "2 symbols in src/a.rs\n1:4  struct Outer\n2:5    method inner"
        );
        assert_eq!(
            document_symbols(&mut Lines::new(&ws), &path, &json!([])),
            "no symbols"
        );
    }

    #[test]
    fn flat_symbol_information_and_workspace_symbols_include_containers() {
        let (_dir, ws) = project();
        let file = uri(&ws, "src/a.rs");
        let flat =
            json!([{"name": "alpha", "kind": 12, "location": {"uri": file, "range": at(0, 3)}}]);
        let path = ws.root().join("src/a.rs");
        assert_eq!(
            document_symbols(&mut Lines::new(&ws), &path, &flat),
            "1 symbols in src/a.rs\n1:4  function alpha"
        );
        let found = json!([
            {"name": "alpha", "kind": 12, "containerName": "util", "location": {"uri": file, "range": at(0, 3)}},
            {"name": "Beta", "kind": 5, "location": {"uri": file}},
        ]);
        assert_eq!(
            workspace_symbols(&mut Lines::new(&ws), &found),
            "2 symbols\nsrc/a.rs:1:4  function alpha (util)\nsrc/a.rs  class Beta"
        );
    }

    #[test]
    fn diagnostics_are_sorted_labelled_and_counted() {
        let (_dir, ws) = project();
        let path = ws.root().join("src/a.rs");
        let items = vec![
            json!({"range": at(1, 18), "severity": 2, "source": "rustc", "code": "unused", "message": "unused variable\nhelp: prefix"}),
            json!({"range": at(0, 0), "severity": 1, "code": 308, "message": "mismatched types"}),
        ];
        let text = diagnostics(&mut Lines::new(&ws), &path, &items);
        assert_eq!(
            text,
            "2 diagnostics in src/a.rs (1 errors, 1 warnings)\n\
             src/a.rs:1:1  error[308] mismatched types\n\
             src/a.rs:2:18  warning[rustc unused] unused variable / help: prefix"
        );
        assert_eq!(
            diagnostics(&mut Lines::new(&ws), &path, &[]),
            "no diagnostics for src/a.rs"
        );
    }

    #[test]
    fn workspace_edit_lists_edits_without_applying_them() {
        let (_dir, ws) = project();
        let file = uri(&ws, "src/a.rs");
        let before = fs::read_to_string(ws.root().join("src/a.rs")).unwrap();
        let edit = json!({"documentChanges": [
            {"textDocument": {"uri": file, "version": 1}, "edits": [
                {"range": {"start": {"line": 1, "character": 18}, "end": {"line": 1, "character": 22}}, "newText": "gamma"},
                {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 8}}, "newText": "a\nb"},
            ]},
            {"kind": "rename", "oldUri": file, "newUri": uri(&ws, "src/b.rs")},
        ]});
        let text = workspace_edit(&mut Lines::new(&ws), &edit);
        assert_eq!(
            text,
            "rename preview, NOT applied: 2 edits in 1 files. Apply them with the edit tool.\n\
             rename src/a.rs -> src/b.rs\n\
             src/a.rs:1:4-1:9 -> \"a\\nb\"  | fn alpha() {}\n\
             src/a.rs:2:18-2:22 -> \"gamma\"  | let s = \"😀\"; beta();"
        );
        assert_eq!(
            fs::read_to_string(ws.root().join("src/a.rs")).unwrap(),
            before
        );
    }

    #[test]
    fn workspace_edit_accepts_the_legacy_changes_map_and_reports_empty() {
        let (_dir, ws) = project();
        let file = uri(&ws, "src/a.rs");
        let edit = json!({"changes": {file: [{"range": at(0, 3), "newText": "x"}]}});
        assert!(workspace_edit(&mut Lines::new(&ws), &edit).contains("1 edits in 1 files"));
        assert_eq!(
            workspace_edit(&mut Lines::new(&ws), &json!({})),
            "the server proposes no changes"
        );
        assert_eq!(
            workspace_edit(&mut Lines::new(&ws), &Value::Null),
            "the server proposes no changes"
        );
    }
}

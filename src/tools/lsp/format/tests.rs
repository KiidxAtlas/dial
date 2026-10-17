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
    let flat = json!([{"name": "alpha", "kind": 12, "location": {"uri": file, "range": at(0, 3)}}]);
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

use crate::tools::edit::hashline;
use crate::tools::testkit::*;
use serde_json::json;
use std::fs;

const SOURCE: &str = "pub struct Foo {\n    a: u8,\n}\n\nimpl Foo {\n    pub fn new() -> Self {\n        Foo { a: 1 }\n    }\n}\n\nfn helper() {}\n";

#[test]
fn outline_lists_nested_declarations_with_line_ranges() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.rs"), SOURCE).unwrap();
    let out = run(&ws, "read", json!({"path": "f.rs:outline"}));
    assert!(!out.is_error, "{}", out.text);
    let rows: Vec<_> = out.text.lines().skip(1).collect();
    assert_eq!(
        rows,
        [
            "1-3 struct pub struct Foo {",
            "5-9 impl impl Foo {",
            "  6-8 function pub fn new() -> Self {",
            "11-11 function fn helper() {}",
        ],
        "{}",
        out.text
    );
    assert!(
        out.text
            .starts_with("f.rs outline (4 declarations, 11 lines;")
    );
    assert!(out.text.contains("not edit witnesses"), "{}", out.text);
    // Works per target inside a multi-read.
    let several = run(&ws, "read", json!({"paths": ["f.rs:outline", "f.rs:11"]}));
    assert!(
        several.text.contains("  6-8 function") && several.text.contains("fn helper() {}"),
        "{}",
        several.text
    );
}

#[test]
fn outline_rows_do_not_establish_edit_witnesses() {
    for mode in [
        crate::tools::EditMode::Replace,
        crate::tools::EditMode::Hashline,
    ] {
        let (dir, ws) = workspace();
        let ws = ws.with_edit_mode(mode);
        fs::write(dir.path().join("f.rs"), SOURCE).unwrap();
        let path = ws.resolve("f.rs").unwrap();
        let lf = hashline::normalize(SOURCE);
        assert!(!run(&ws, "read", json!({"path": "f.rs:outline"})).is_error);
        assert!(
            ws.store().check_ranges(&path, &lf, &[(6, 8)]).is_err(),
            "outline rows granted an edit witness in {mode:?}"
        );
        assert!(!run(&ws, "read", json!({"path": "f.rs:6-8"})).is_error);
        assert!(ws.store().check_ranges(&path, &lf, &[(6, 8)]).is_ok());
    }
}

#[test]
fn outline_rejects_unsupported_files_and_line_arguments_and_names_missing_paths() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("notes.txt"), "plain\n").unwrap();
    fs::write(dir.path().join("f.rs"), SOURCE).unwrap();
    let unsupported = run(&ws, "read", json!({"path": "notes.txt:outline"}));
    assert!(
        unsupported.is_error
            && unsupported.text.contains("no syntax parser")
            && unsupported.text.contains("`notes.txt:1-200`"),
        "{}",
        unsupported.text
    );
    let offset = run(&ws, "read", json!({"path": "f.rs:outline", "offset": 2}));
    assert!(
        offset.is_error && offset.text.contains("offset/limit"),
        "{}",
        offset.text
    );
    let missing = run(&ws, "read", json!({"path": "g.rs:outline"}));
    assert!(
        missing.is_error
            && missing.text.contains("no such file")
            && missing.text.contains("Did you mean: f.rs"),
        "{}",
        missing.text
    );
    // A file literally named like the selector is read as text.
    fs::write(dir.path().join("odd:outline"), "literal\n").unwrap();
    let literal = run(&ws, "read", json!({"path": "odd:outline"}));
    assert!(literal.text.contains("literal"), "{}", literal.text);
}

const DOCUMENTED: &str = "use std::fmt;\n\n/// Builds things.\n#[derive(Debug)]\npub struct Foo {\n    a: u8,\n}\n\nimpl Foo {\n    /// Make one.\n    pub fn new() -> Self {\n        Foo { a: 1 }\n    }\n}\n\nfn helper() {}\n";

#[test]
fn symbol_selector_reads_a_declaration_with_its_doc_comments() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.rs"), DOCUMENTED).unwrap();
    let out = run(&ws, "read", json!({"path": "f.rs:sym=Foo.new"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("/// Make one."), "{}", out.text);
    assert!(out.text.contains("pub fn new() -> Self"), "{}", out.text);
    assert!(!out.text.contains("fn helper"), "{}", out.text);
    // A struct brings its doc comment and attribute along.
    let foo = run(&ws, "read", json!({"path": "f.rs:sym=Foo"}));
    assert!(foo.text.contains("/// Builds things."), "{}", foo.text);
    assert!(foo.text.contains("#[derive(Debug)]"), "{}", foo.text);
    let missing = run(&ws, "read", json!({"path": "f.rs:sym=nothing"}));
    assert!(missing.is_error, "{}", missing.text);
    assert!(missing.text.contains("f.rs:outline"), "{}", missing.text);
    let mixed = run(&ws, "read", json!({"path": "f.rs:sym=helper", "offset": 3}));
    assert!(mixed.is_error, "{}", mixed.text);
}

#[test]
fn outline_expand_appends_the_named_declarations() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.rs"), DOCUMENTED).unwrap();
    let out = run(&ws, "read", json!({"path": "f.rs:outline=helper"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.starts_with("f.rs outline ("), "{}", out.text);
    assert!(out.text.contains("16:fn helper() {}"), "{}", out.text);
}

#[test]
fn directory_outline_lists_top_level_declarations_per_file() {
    let (dir, ws) = workspace();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), SOURCE).unwrap();
    fs::write(dir.path().join("src/notes.txt"), "text\n").unwrap();
    let out = run(&ws, "read", json!({"path": "src:outline"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("src/a.rs (11 lines)"), "{}", out.text);
    assert!(out.text.contains("  5-9 impl impl Foo {"), "{}", out.text);
    // Only top-level declarations: the nested method is not listed.
    assert!(!out.text.contains("6-8 function"), "{}", out.text);
    assert!(
        out.text.contains("1 files without a syntax parser"),
        "{}",
        out.text
    );
}

use crate::tools::testkit::*;
use crate::tools::{EditMode, Workspace};
use serde_json::json;
use std::fs;

/// A Rust file with `functions` functions of `body` statements each, plus imports and a struct.
fn source(functions: usize, body: usize) -> String {
    let mut text = String::from("use std::fmt;\n\npub struct Holder {\n    value: u32,\n}\n\n");
    for n in 0..functions {
        text.push_str(&format!("pub fn work_{n}(input: u32) -> u32 {{\n"));
        for line in 0..body {
            text.push_str(&format!("    let step_{line} = input + {line};\n"));
        }
        text.push_str("    input\n}\n\n");
    }
    text
}

fn hashline_workspace() -> (tempfile::TempDir, Workspace) {
    let (dir, ws) = workspace();
    (dir, ws.with_edit_mode(EditMode::Hashline))
}

#[test]
fn oversized_source_is_summarized_with_real_line_numbers_and_markers() {
    let (dir, ws) = hashline_workspace();
    let text = source(40, 60);
    fs::write(dir.path().join("big.rs"), &text).unwrap();
    assert!(text.lines().count() > 2000);

    let out = run(&ws, "read", json!({"path": "big.rs"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("summarized view"), "{}", out.text);
    assert!(out.text.len() < 48 * 1024);
    assert!(
        out.text
            .contains("\n7:pub fn work_0(input: u32) -> u32 {\n")
    );
    assert!(
        out.text
            .contains("\n  ⋮ lines 8-68 elided (read big.rs:8-68)\n"),
        "{}",
        out.text
    );
    assert!(out.text.contains("\n69:}\n"), "{}", out.text);
    assert!(out.text.contains("\n1:use std::fmt;\n"));
    assert!(
        !out.text.contains("step_30"),
        "elided body rows must not appear"
    );
    assert!(
        out.text.contains("big.rs:1-"),
        "footer names how to read the full file"
    );
}

#[test]
fn elided_lines_are_not_witnessed_but_shown_lines_are() {
    let (dir, ws) = hashline_workspace();
    let text = source(40, 60);
    fs::write(dir.path().join("big.rs"), &text).unwrap();
    let out = run(&ws, "read", json!({"path": "big.rs"})).text;
    assert!(out.contains("elided"));

    let abs = ws.resolve("big.rs").unwrap();
    let lf = crate::tools::edit::hashline::normalize(&text);
    let store = ws.store();
    assert!(
        store.check_ranges(&abs, &lf, &[(7, 7)]).is_ok(),
        "signature line was shown"
    );
    assert!(
        store.check_ranges(&abs, &lf, &[(30, 30)]).is_err(),
        "elided line has no witness"
    );
}

#[test]
fn small_files_explicit_ranges_and_unsupported_types_read_normally() {
    let (dir, ws) = hashline_workspace();
    fs::write(dir.path().join("small.rs"), source(2, 20)).unwrap();
    let small = run(&ws, "read", json!({"path": "small.rs"})).text;
    assert!(
        !small.contains("summarized") && !small.contains('⋮'),
        "{small}"
    );

    fs::write(dir.path().join("big.rs"), source(40, 60)).unwrap();
    for args in [
        json!({"path": "big.rs:1-30"}),
        json!({"path": "big.rs", "offset": 8, "limit": 20}),
        json!({"path": "big.rs", "limit": 100}),
        json!({"path": "big.rs:raw"}),
    ] {
        let out = run(&ws, "read", args.clone()).text;
        assert!(!out.contains("summarized view"), "{args}: {out}");
    }

    let notes: String = (1..=5000).map(|n| format!("note {n}\n")).collect();
    fs::write(dir.path().join("notes.txt"), notes).unwrap();
    let out = run(&ws, "read", json!({"path": "notes.txt"})).text;
    assert!(
        out.contains("continue with `notes.txt:2001-`") && !out.contains("summarized"),
        "{out}"
    );
}

#[test]
fn a_file_with_nothing_to_elide_falls_back_to_the_truncated_read() {
    let (dir, ws) = hashline_workspace();
    let flat: String = (0..4000)
        .map(|n| format!("pub const C{n}: u32 = {n};\n"))
        .collect();
    fs::write(dir.path().join("flat.rs"), flat).unwrap();
    let out = run(&ws, "read", json!({"path": "flat.rs"})).text;
    assert!(!out.contains("summarized view"), "{out}");
    assert!(out.contains("continue with `flat.rs:"), "{out}");
}

#[test]
fn python_indented_bodies_are_elided_and_replace_mode_stays_plain() {
    let (dir, ws) = hashline_workspace();
    let mut py = String::from("import os\n\n");
    for n in 0..60 {
        py.push_str(&format!("def f{n}(x):\n"));
        for k in 0..60 {
            py.push_str(&format!("    y{k} = x + {k}\n"));
        }
        py.push_str("    return x\n\n");
    }
    fs::write(dir.path().join("m.py"), &py).unwrap();
    let out = run(&ws, "read", json!({"path": "m.py"})).text;
    assert!(
        out.contains("summarized view") && out.contains("⋮ lines 4-"),
        "{out}"
    );
    assert!(out.contains("\n3:def f0(x):\n"), "{out}");

    let (dir, ws) = workspace();
    fs::write(dir.path().join("big.rs"), source(40, 60)).unwrap();
    let plain = run(&ws, "read", json!({"path": "big.rs"})).text;
    assert!(!plain.contains("summarized view"));
}

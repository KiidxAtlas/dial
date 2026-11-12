use super::*;
use crate::tools::EditMode;
use crate::tools::testkit::*;
use serde_json::json;
use std::fs;

const LICENSED: &str = "// Copyright 2024 Example\n//\n// Licensed under the Apache License, Version 2.0\n// you may not use this file except in compliance\n// with the License.\n// SPDX-License-Identifier: Apache-2.0\n\nfn main() {}\n";

#[test]
fn licence_headers_and_long_import_runs_fold() {
    let lines = hashline::split_lines(LICENSED);
    assert_eq!(folds(&lines), vec![(1, 6, "licence header")]);
    let mut imports = String::new();
    for n in 0..10 {
        imports.push_str(&format!("use crate::m{n};\n"));
    }
    imports.push_str("use std::{\n    fs,\n    io,\n};\n\nfn f() {}\n");
    let lines = hashline::split_lines(&imports);
    assert_eq!(folds(&lines), vec![(1, 14, "imports")]);
    // Short runs and ordinary comments stay.
    let short = "use a;\nuse b;\n// a note\n// more\nfn f() {}\n";
    assert!(folds(&hashline::split_lines(short)).is_empty());
}

#[test]
fn whole_file_reads_fold_and_ranges_show_folded_lines() {
    let (dir, ws) = workspace();
    let ws = ws.with_edit_mode(EditMode::Hashline);
    fs::write(dir.path().join("m.rs"), LICENSED).unwrap();
    let whole = run(&ws, "read", json!({"path": "m.rs"}));
    assert!(!whole.is_error, "{}", whole.text);
    assert!(!whole.text.contains("Apache"), "{}", whole.text);
    assert!(whole.text.contains("8:fn main() {}"), "{}", whole.text);
    assert!(
        whole.text.contains("[Folded lines 1-6 licence header"),
        "{}",
        whole.text
    );
    let range = run(&ws, "read", json!({"path": "m.rs:1-8"}));
    assert!(
        range.text.contains("3:// Licensed under the Apache"),
        "{}",
        range.text
    );
    assert!(!range.text.contains("[Folded"), "{}", range.text);
}

#[test]
fn budget_lowers_the_output_cap() {
    let (dir, ws) = workspace();
    let body: String = (1..=500).map(|n| format!("row number {n}\n")).collect();
    fs::write(dir.path().join("big.txt"), body).unwrap();
    let small = run(
        &ws,
        "read",
        json!({"path": "big.txt:1-500", "budget": 1500}),
    );
    assert!(!small.is_error, "{}", small.text);
    assert!(small.text.len() <= 1500, "{}", small.text.len());
    assert!(!small.text.contains("row number 500"), "{}", small.text);
    let refused = run(&ws, "read", json!({"path": "big.txt", "budget": 10}));
    assert!(refused.is_error, "{}", refused.text);
}

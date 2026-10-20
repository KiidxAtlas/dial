use crate::tools::testkit::*;
use serde_json::json;
use std::fs;

fn put(root: &std::path::Path, relative: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "a\n").unwrap();
}

fn did_you_mean(text: &str) -> Vec<&str> {
    text.lines()
        .find_map(|line| line.strip_prefix("Did you mean: "))
        .map(|list| list.split(", ").collect())
        .unwrap_or_default()
}

#[test]
fn missing_read_keeps_wording_and_ranks_close_paths_without_private_entries() {
    let (dir, ws) = workspace();
    let ws = ws.with_protected_paths(&[dir.path().join("state")]);
    for relative in [
        "src/tools/mod.rs",
        "src/tool/registry.rs",
        "lib/registry.rs",
        "docs/REGISTRY.md",
        // Never suggested: private, secret, policy, dependency and ignored entries.
        ".dial/registry.rs",
        "state/registry.rs",
        "keys/registry.pem",
        ".git/registry.rs",
        "target/registry.rs",
        "node_modules/registry.rs",
        "out/registry.rs",
    ] {
        put(dir.path(), relative);
    }
    fs::write(dir.path().join(".gitignore"), "out/\n").unwrap();
    let missing = run(&ws, "read", json!({"path": "src/tools/registry.rs"}));
    assert!(missing.is_error, "{}", missing.text);
    assert!(
        missing
            .text
            .starts_with("reading \"src/tools/registry.rs\": no such file or directory\n"),
        "{}",
        missing.text
    );
    assert_eq!(
        did_you_mean(&missing.text),
        [
            "src/tool/registry.rs",
            "lib/registry.rs",
            "docs/REGISTRY.md"
        ],
        "{}",
        missing.text
    );
    // A selector suffix is not part of the looked-up name; multi-read reports inline.
    let several = run(
        &ws,
        "read",
        json!({"paths": ["src/tools/mod.rs", "src/tools/registry.rs:1-5"]}),
    );
    assert!(
        several
            .text
            .contains("no such file or directory\nDid you mean: src/tool/registry.rs,"),
        "{}",
        several.text
    );
}

#[test]
fn directory_typos_suggest_the_corrected_path_and_sibling_directory() {
    let (dir, ws) = workspace();
    put(dir.path(), "src/tools/deep/a.rs");
    put(dir.path(), "src/other.rs");
    let missing = run(&ws, "read", json!({"path": "src/tolls/deep/a.rs"}));
    let shown = did_you_mean(&missing.text);
    assert_eq!(
        shown.first(),
        Some(&"src/tools/deep/a.rs"),
        "{}",
        missing.text
    );
    assert!(shown.contains(&"src/tools/"), "{}", missing.text);
    assert!(!shown.contains(&"src/other.rs"), "{}", missing.text);
}

#[test]
fn search_scopes_and_edit_targets_get_suggestions_and_unrelated_misses_stay_unchanged() {
    let (dir, ws) = workspace();
    put(dir.path(), "lib/registry.rs");
    let grep = run(&ws, "grep", json!({"pattern": "a", "path": "lbi"}));
    assert!(
        grep.is_error && grep.text.contains("no such file"),
        "{}",
        grep.text
    );
    assert!(did_you_mean(&grep.text).contains(&"lib/"), "{}", grep.text);
    let glob = run(
        &ws,
        "glob",
        json!({"pattern": "*.rs", "path": "lib/registy.rs"}),
    );
    assert_eq!(
        did_you_mean(&glob.text),
        ["lib/registry.rs"],
        "{}",
        glob.text
    );
    let edit = run(
        &ws,
        "edit",
        json!({"path": "lib/registy.rs", "old_string": "a", "new_string": "b"}),
    );
    assert!(edit.is_error, "{}", edit.text);
    assert_eq!(
        did_you_mean(&edit.text),
        ["lib/registry.rs"],
        "{}",
        edit.text
    );
    let unrelated = run(&ws, "read", json!({"path": "zzzzzzzz/qqqqqqqq.toml"}));
    assert!(
        unrelated.is_error
            && unrelated.text.contains("no such file")
            && !unrelated.text.contains("Did you mean"),
        "{}",
        unrelated.text
    );
}

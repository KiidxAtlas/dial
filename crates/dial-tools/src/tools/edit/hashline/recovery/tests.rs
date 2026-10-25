use super::super::{edit, tag_of};
use crate::tools::Workspace;
use std::fs;

const MISPLACED: &str = "would likely misplace it";

/// Workspace holding `name` as `base`, fully read; then the file on disk becomes `current`.
fn stale(name: &str, base: &str, current: &str) -> (tempfile::TempDir, Workspace, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve(name).unwrap();
    fs::write(&path, base).unwrap();
    let tag = ws.store().record_all(&path, base);
    assert_eq!(tag, tag_of(base));
    fs::write(&path, current).unwrap();
    (dir, ws, tag, name.to_owned())
}

fn contents(ws: &Workspace, name: &str) -> String {
    fs::read_to_string(ws.resolve(name).unwrap()).unwrap()
}

#[test]
fn anchor_remapped_into_a_sibling_function_is_refused() {
    // `beta` was deleted and `alpha` renamed: the suffix diff aligns beta's `work();` with
    // alpha_renamed's identical row at the same offset, so only the enclosing function differs.
    let base = "fn alpha() {\n    work();\n}\nfn beta() {\n    work();\n}\n";
    let current = "fn alpha_renamed() {\n    work();\n}\n";
    let (_dir, ws, tag, name) = stale("a.rs", base, current);
    let error = edit(&ws, &format!("[{name}#{tag}]\nPUT 5.=5:\n+    other();\n")).unwrap_err();
    assert!(error.contains(MISPLACED), "{error}");
    assert!(error.contains("enclosing block"), "{error}");
    assert!(error.contains(&tag_of(current)), "fresh tag named: {error}");
    assert_eq!(contents(&ws, &name), current);
}

#[test]
fn indentation_context_guards_files_without_a_grammar() {
    let base = "alpha:\n  work step\nbeta:\n  work step\n";
    let current = "alpha_renamed:\n  work step\n";
    let (_dir, ws, tag, name) = stale("a.txt", base, current);
    let error = edit(&ws, &format!("[{name}#{tag}]\nPUT 4.=4:\n+  other step\n")).unwrap_err();
    assert!(error.contains(MISPLACED), "{error}");
    assert_eq!(contents(&ws, &name), current);
}

#[test]
fn insertion_above_the_target_still_recovers() {
    let base = "fn alpha() {\n    work();\n}\nfn beta() {\n    work();\n}\n";
    let current = format!("use std::fmt;\n{base}");
    let (_dir, ws, tag, name) = stale("a.rs", base, &current);
    let out = edit(&ws, &format!("[{name}#{tag}]\nPUT 5.=5:\n+    other();\n")).unwrap();
    assert!(out.contains("stale"), "{out}");
    assert_eq!(
        contents(&ws, &name),
        "use std::fmt;\nfn alpha() {\n    work();\n}\nfn beta() {\n    other();\n}\n"
    );
}

#[test]
fn insertion_gap_inside_the_same_block_still_recovers() {
    let base = "fn alpha() {\n    work();\n}\nfn beta() {\n    work();\n}\n";
    let current = format!("use std::fmt;\n{base}");
    let (_dir, ws, tag, name) = stale("a.rs", base, &current);
    edit(&ws, &format!("[{name}#{tag}]\nPUT >5:\n+    after();\n")).unwrap();
    assert_eq!(
        contents(&ws, &name),
        "use std::fmt;\nfn alpha() {\n    work();\n}\nfn beta() {\n    work();\n    after();\n}\n"
    );
}

const PROSE: [&str; 10] = [
    "configure the retry policy for upstream calls",
    "parse the manifest before touching the cache",
    "spawn a watcher that flushes pending writes",
    "validate every credential against the registry",
    "render the summary table with aligned columns",
    "compress archived logs older than one week",
    "negotiate protocol versions with the server",
    "schedule nightly index rebuilds after backups",
    "emit telemetry events for failed migrations",
    "release the lease when the worker shuts down",
];

fn numbered_prose() -> String {
    PROSE.iter().map(|row| format!("{row}\n")).collect()
}

#[test]
fn numbers_copied_from_the_current_file_are_not_shifted() {
    let base = numbered_prose();
    let current = format!("a new first line of the file\nanother new second line\n{base}");
    // Line 6 of the current file is what the model saw (grep -n), but the tag is the old one.
    let aimed = "validate every credential against the registry, twice\n";
    let (_dir, ws, tag, name) = stale("a.txt", &base, &current);
    let error = edit(&ws, &format!("[{name}#{tag}]\nPUT 6.=6:\n+{}", aimed)).unwrap_err();
    assert!(error.contains(MISPLACED), "{error}");
    assert!(error.contains("numbers you gave"), "{error}");
    assert_eq!(contents(&ws, &name), current);
}

#[test]
fn unrelated_replacement_text_is_remapped_despite_a_shift() {
    let base = numbered_prose();
    let current = format!("a new first line of the file\nanother new second line\n{base}");
    let (_dir, ws, tag, name) = stale("a.txt", &base, &current);
    edit(
        &ws,
        &format!("[{name}#{tag}]\nPUT 6.=6:\n+completely different replacement\n"),
    )
    .unwrap();
    let after = contents(&ws, &name);
    assert!(after.contains(PROSE[3]), "line above the target kept");
    assert!(!after.contains(PROSE[5]), "base line 6 replaced");
    assert!(after.contains("completely different replacement"));
}

use super::engine::lang_names;
use super::*;
use crate::config::types::EditMode;
use crate::tools::{WriteObserver, execute};
use anyhow::{Result, bail};
use parking_lot::Mutex;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

fn workspace() -> (tempfile::TempDir, Workspace) {
    crate::tools::testkit::workspace()
}

fn call(ws: &Workspace, tool: &dyn Tool, args: Value) -> ToolOutput {
    let config = Config::default();
    let original = crate::llm::ToolCall {
        id: "fixture".into(),
        name: tool.name().into(),
        arguments: args,
        malformed_arguments: false,
    };
    let authorize = crate::tools::testkit::authorize_in_workspace(ws, tool.name());
    tool.call(
        &ToolContext {
            workspace: ws,
            config: &config,
            mutation_authorizer: Some(&authorize),
            confirm: None,
        },
        &original.arguments,
    )
}

fn search(ws: &Workspace, args: Value) -> ToolOutput {
    call(ws, &AstSearch, args)
}

fn edit(ws: &Workspace, args: Value) -> ToolOutput {
    call(ws, &AstEdit, args)
}

fn put(dir: &tempfile::TempDir, rel: &str, text: &str) -> PathBuf {
    let path = dir.path().join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    path
}

/// Records the path and the on-disk content at the moment of each call; fails for `deny`.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<(PathBuf, String)>>,
    deny: Option<&'static str>,
}

impl WriteObserver for Recorder {
    fn before_write(&self, abs: &Path) -> Result<()> {
        if self.deny.is_some_and(|name| abs.ends_with(name)) {
            bail!("checkpoint refused");
        }
        let content = fs::read_to_string(abs).unwrap_or_default();
        self.seen.lock().push((abs.to_owned(), content));
        Ok(())
    }
}

#[test]
fn search_infers_language_per_file_and_reports_position_and_captures() {
    let (dir, ws) = workspace();
    put(&dir, "a.rs", "fn main() {\n    let x = add(1, 2);\n}\n");
    put(&dir, "b.py", "def run():\n    return add(3, 4)\n");
    put(&dir, "c.ts", "const y = add(5, 6);\n");
    put(&dir, "notes.txt", "add(7, 8)\n");

    let out = search(&ws, json!({"pattern": "add($A, $B)"}));
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("a.rs:2:13: "), "{}", out.text);
    assert!(out.text.contains("let x = add(1, 2);"));
    assert!(out.text.contains("$A = 1") && out.text.contains("$B = 2"));
    assert!(out.text.contains("b.py:2:12: "), "{}", out.text);
    assert!(out.text.contains("c.ts:1:11: "), "{}", out.text);
    assert!(!out.text.contains("notes.txt"));
    assert!(out.text.contains("[3 matches in 3 files]"));
}

#[test]
fn search_captures_runs_of_nodes_and_honours_lang_and_limit() {
    let (dir, ws) = workspace();
    put(&dir, "a.rs", "fn f() { g(1, 2, 3); g(4); g(); }\n");
    put(&dir, "b.py", "g(9)\n");

    let out = search(
        &ws,
        json!({"pattern": "g($FIRST, $$$REST)", "lang": "rust"}),
    );
    assert!(
        out.text.contains("$FIRST = 1") && out.text.contains("$$$REST = 2, 3"),
        "{}",
        out.text
    );
    assert!(
        !out.text.contains("b.py"),
        "lang must restrict the files searched"
    );

    let limited = search(
        &ws,
        json!({"pattern": "g($$$A)", "lang": "rust", "limit": 2}),
    );
    assert_eq!(limited.text.matches("a.rs:").count(), 2, "{}", limited.text);
    assert!(limited.text.contains("stopped at the match limit"));

    let none = search(&ws, json!({"pattern": "nothing($A)"}));
    assert!(!none.is_error && none.text.starts_with("no matches"));
}

#[test]
fn search_single_file_uses_lang_override_and_rejects_unknowns() {
    let (dir, ws) = workspace();
    put(&dir, "script", "print(1)\n");
    put(&dir, "plain.txt", "print(2)\n");
    let out = search(
        &ws,
        json!({"pattern": "print($A)", "path": "script", "lang": "python"}),
    );
    assert!(out.text.contains("script:1:1: print(1)"), "{}", out.text);
    let no_lang = search(&ws, json!({"pattern": "print($A)", "path": "plain.txt"}));
    assert!(no_lang.is_error && no_lang.text.contains("cannot infer the language"));
    let bad = search(&ws, json!({"pattern": "print($A)", "lang": "klingon"}));
    assert!(bad.is_error && bad.text.contains("unsupported language"));
}

#[test]
fn explicit_unloadable_files_are_errors_for_search_and_edit() {
    let (dir, ws) = workspace();
    for (name, bytes, reason) in [
        ("large.rs", vec![b'a'; 2 * 1024 * 1024 + 1], "2 MiB"),
        ("binary.rs", b"fn f() {}\0".to_vec(), "binary"),
        ("invalid.rs", vec![0xff], "UTF-8"),
    ] {
        fs::write(dir.path().join(name), bytes).unwrap();
        for out in [
            search(&ws, json!({"path": name, "pattern": "f()"})),
            edit(
                &ws,
                json!({"path": name, "pattern": "f()", "rewrite": "g()"}),
            ),
        ] {
            assert!(
                out.is_error && out.text.contains(name) && out.text.contains(reason),
                "{}",
                out.text
            );
        }
    }
}

#[test]
fn explicit_file_removed_after_resolution_reports_io_error() {
    let (dir, ws) = workspace();
    let path = put(&dir, "gone.rs", "fn f() {}\n");
    let target = Target::resolve(&ws, Some("gone.rs"), None).unwrap();
    fs::remove_file(path).unwrap();
    let mut queries = Queries::new("f()", None);
    let error = match scan(&ws, &target, &mut queries, &mut |_, _| true) {
        Err(error) => error,
        Ok(_) => panic!("missing target must fail"),
    };
    assert!(error.contains("gone.rs"), "{error}");
}

#[cfg(unix)]
#[test]
fn tree_reports_unreadable_source_as_unreadable() {
    let (dir, ws) = workspace();
    let path = put(&dir, "blocked.rs", "fn f() {}\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    if fs::File::open(&path).is_ok() {
        return; // Privileged users may bypass permissions.
    }
    let out = search(&ws, json!({"pattern": "f()"}));
    assert!(
        !out.is_error && out.text.contains("1 unreadable files skipped"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("binary"), "{}", out.text);
}

#[test]
fn tree_coverage_distinguishes_unreadable_files_from_content_skips() {
    let (dir, ws) = workspace();
    put(&dir, "a.rs", "fn a() {}\n");
    put(&dir, "b.rs", "fn b() {}\n");
    let target = Target::resolve(&ws, None, Some("rust")).unwrap();
    let mut queries = Queries::new("fn $F() {}", None);
    let coverage = scan(&ws, &target, &mut queries, &mut |file, _| {
        if file.rel == "a.rs" {
            fs::remove_file(dir.path().join("b.rs")).unwrap();
        }
        true
    })
    .unwrap();
    assert!(
        coverage.footer().contains("incomplete"),
        "{}",
        coverage.footer()
    );
    assert!(
        !coverage.footer().contains("binary"),
        "{}",
        coverage.footer()
    );
}

#[test]
fn invalid_patterns_are_reported_not_ignored() {
    let (dir, ws) = workspace();
    put(&dir, "a.rs", "fn f() {}\n");
    let broken = search(&ws, json!({"pattern": "fn (", "lang": "rust"}));
    assert!(broken.is_error, "{}", broken.text);
    let multi = search(
        &ws,
        json!({"pattern": "fn a() {} fn b() {}", "lang": "rust"}),
    );
    assert!(multi.is_error, "{}", multi.text);
    let auto = search(&ws, json!({"pattern": "fn ("}));
    assert!(
        auto.is_error && auto.text.contains("does not compile for Rust"),
        "{}",
        auto.text
    );
}

#[test]
fn auto_detection_notes_languages_the_pattern_does_not_fit() {
    let (dir, ws) = workspace();
    put(&dir, "a.rs", "fn main() { let v = vec![1]; }\n");
    put(&dir, "b.py", "x = 1\n");
    // Valid Rust, but not a single Python node.
    let out = search(&ws, json!({"pattern": "fn $F() { $$$B }"}));
    assert!(out.text.contains("a.rs:1:1: "), "{}", out.text);
    assert!(out.text.contains("[Python files skipped:"), "{}", out.text);
}

#[cfg(unix)]
#[test]
fn walk_skips_build_dirs_scratch_binaries_and_symlinks() {
    let (dir, ws) = workspace();
    put(&dir, "src/a.rs", "fn f() { go(); }\n");
    for skipped in ["target/x.rs", "node_modules/y.rs", ".git/z.rs", ".tmp/s.rs"] {
        put(&dir, skipped, "fn f() { go(); }\n");
    }
    put(&dir, "bin.rs", "fn f() { go(); }\n\0");
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("leak.rs"), "fn f() { go(); }\n").unwrap();
    symlink(outside.path(), dir.path().join("linkdir")).unwrap();
    symlink(outside.path().join("leak.rs"), dir.path().join("link.rs")).unwrap();

    let out = search(&ws, json!({"pattern": "go()"}));
    assert!(out.text.contains("src/a.rs:1:"), "{}", out.text);
    assert_eq!(out.text.matches(".rs:").count(), 1, "{}", out.text);
    assert!(
        out.text
            .contains("1 binary, oversized or non-UTF-8 files skipped")
    );
    let direct = search(&ws, json!({"pattern": "go()", "path": "linkdir"}));
    assert!(direct.is_error, "{}", direct.text);
}

#[test]
fn paths_outside_the_workspace_are_refused() {
    let (_dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("a.rs"), "fn f() { go(); }\n").unwrap();
    for path in ["..", "../x", outside.path().to_str().unwrap()] {
        assert!(
            search(&ws, json!({"pattern": "go()", "path": path})).is_error,
            "search {path}"
        );
        let rewrite = edit(
            &ws,
            json!({"pattern": "go()", "rewrite": "stop()", "path": path, "dry_run": false}),
        );
        assert!(rewrite.is_error, "edit {path}");
    }
    assert_eq!(
        fs::read_to_string(outside.path().join("a.rs")).unwrap(),
        "fn f() { go(); }\n"
    );
}

#[test]
fn dry_run_previews_a_diff_and_apply_writes() {
    let (dir, ws) = workspace();
    let a = put(
        &dir,
        "a.rs",
        "fn main() {\n    old_name(1);\n    keep();\n    old_name(2);\n}\n",
    );
    let b = put(&dir, "b.rs", "fn other() { old_name(3); }\n");
    let args = json!({"pattern": "old_name($A)", "rewrite": "new_name($A, true)"});

    let preview = edit(&ws, args.clone());
    assert!(!preview.is_error, "{}", preview.text);
    assert!(
        preview
            .text
            .starts_with("dry run: 3 replacements in 2 files"),
        "{}",
        preview.text
    );
    assert!(
        preview.text.contains(
            "--- a/a.rs\n+++ b/a.rs\n@@ -2,1 +2,1 @@\n-    old_name(1);\n+    new_name(1, true);\n"
        ),
        "{}",
        preview.text
    );
    assert!(
        preview
            .text
            .contains("@@ -4,1 +4,1 @@\n-    old_name(2);\n+    new_name(2, true);\n")
    );
    assert!(
        fs::read_to_string(&a).unwrap().contains("old_name(1)"),
        "dry run must not write"
    );
    assert!(!AstEdit.needs_approval(&args));
    assert!(!AstEdit.needs_approval(&json!({"pattern": "p", "rewrite": "r", "dry_run": true})));

    let apply =
        json!({"pattern": "old_name($A)", "rewrite": "new_name($A, true)", "dry_run": false});
    assert!(AstEdit.needs_approval(&apply));
    let done = edit(&ws, apply);
    assert!(!done.is_error, "{}", done.text);
    assert!(
        done.text.contains("applied 3 replacements in 2 files"),
        "{}",
        done.text
    );
    assert_eq!(
        fs::read_to_string(&a).unwrap(),
        "fn main() {\n    new_name(1, true);\n    keep();\n    new_name(2, true);\n}\n"
    );
    assert_eq!(
        fs::read_to_string(&b).unwrap(),
        "fn other() { new_name(3, true); }\n"
    );
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("dial-ast"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn rewrite_may_reorder_and_must_use_known_variables() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.py", "result = combine(left, right)\n");
    let swapped = edit(
        &ws,
        json!({"pattern": "combine($X, $Y)", "rewrite": "combine($Y, $X)", "dry_run": false}),
    );
    assert!(!swapped.is_error, "{}", swapped.text);
    assert_eq!(
        fs::read_to_string(&a).unwrap(),
        "result = combine(right, left)\n"
    );

    let unknown = edit(
        &ws,
        json!({"pattern": "combine($X, $Y)", "rewrite": "combine($Z)", "dry_run": false}),
    );
    assert!(
        unknown.is_error && unknown.text.contains("$Z"),
        "{}",
        unknown.text
    );
}

#[test]
fn rewrite_that_breaks_syntax_is_applied_with_an_advisory() {
    let (dir, ws) = workspace();
    let ws = ws.with_syntax_guard(true);
    let a = put(&dir, "a.rs", "fn f() {\n    foo(1);\n}\n");
    let preview = edit(&ws, json!({"pattern": "foo($A)", "rewrite": "bar("}));
    assert!(!preview.is_error, "{}", preview.text);
    assert!(preview.text.contains("+    bar("), "{}", preview.text);
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar(", "dry_run": false}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Syntax advisory"), "{}", out.text);
    assert_eq!(fs::read_to_string(&a).unwrap(), "fn f() {\n    bar(;\n}\n");
}

#[test]
fn files_that_were_already_broken_may_still_be_rewritten_if_no_new_errors_appear() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.py", "foo(1)\nbroken = (\n");
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar($A)", "dry_run": false}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(fs::read_to_string(&a).unwrap().starts_with("bar(1)\n"));
}

#[test]
fn multi_file_apply_is_not_vetoed_by_syntax_diagnostics() {
    let (dir, ws) = workspace();
    let ws = ws.with_syntax_guard(true);
    let rs = put(&dir, "a.rs", "fn f() { foo(x); }\n");
    let py = put(&dir, "b.py", "foo(x)\n");
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "$A?", "dry_run": false}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("Syntax advisory"), "{}", out.text);
    assert_eq!(fs::read_to_string(&rs).unwrap(), "fn f() { x?; }\n");
    assert_eq!(fs::read_to_string(&py).unwrap(), "x?\n");
}

#[cfg(unix)]
#[test]
fn staging_failure_on_a_later_file_leaves_every_file_untouched() {
    let (dir, ws) = workspace();
    let first = put(&dir, "a/one.rs", "fn f() { foo(1); }\n");
    let second = put(&dir, "b/two.rs", "fn g() { foo(2); }\n");
    let locked = dir.path().join("b");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    if fs::write(locked.join("probe"), "").is_ok() {
        // Running with privileges that ignore directory permissions; the scenario cannot be staged.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: directory permissions are not enforced for this user");
        return;
    }
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar($A)", "dry_run": false}),
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        out.is_error && out.text.contains("nothing was written"),
        "{}",
        out.text
    );
    assert_eq!(fs::read_to_string(&first).unwrap(), "fn f() { foo(1); }\n");
    assert_eq!(fs::read_to_string(&second).unwrap(), "fn g() { foo(2); }\n");
    assert!(
        fs::read_dir(dir.path().join("a"))
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().contains("dial-ast"))
    );
}

#[test]
fn observer_sees_every_file_before_any_is_written() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.rs", "fn f() { foo(1); }\n");
    let b = put(&dir, "b.rs", "fn g() { foo(2); }\n");
    let recorder = Arc::new(Recorder::default());
    let ws = ws.with_observer(recorder.clone());
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar($A)", "dry_run": false}),
    );
    assert!(!out.is_error, "{}", out.text);
    let seen = recorder.seen.lock();
    let canon = |p: &Path| p.canonicalize().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, canon(&a));
    assert_eq!(seen[1].0, canon(&b));
    // Both checkpoints were taken while the files still held their original content.
    assert_eq!(seen[0].1, "fn f() { foo(1); }\n");
    assert_eq!(seen[1].1, "fn g() { foo(2); }\n");

    // A dry run is not a mutation.
    drop(seen);
    let before = recorder.seen.lock().len();
    edit(&ws, json!({"pattern": "bar($A)", "rewrite": "baz($A)"}));
    assert_eq!(recorder.seen.lock().len(), before);
}

#[test]
fn observer_refusal_blocks_the_whole_edit() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.rs", "fn f() { foo(1); }\n");
    let b = put(&dir, "b.rs", "fn g() { foo(2); }\n");
    let ws = ws.with_observer(Arc::new(Recorder {
        deny: Some("b.rs"),
        ..Recorder::default()
    }));
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar($A)", "dry_run": false}),
    );
    assert!(
        out.is_error && out.text.contains("blocked by the harness"),
        "{}",
        out.text
    );
    assert_eq!(fs::read_to_string(&a).unwrap(), "fn f() { foo(1); }\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "fn g() { foo(2); }\n");
}

#[test]
fn hashline_tags_from_before_the_rewrite_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path())
        .unwrap()
        .with_edit_mode(EditMode::Hashline);
    put(&dir, "a.rs", "fn f() { foo(1); }\nfn g() {}\n");
    let read = execute(&ws, "read", &json!({"path": "a.rs"}), 20);
    let tag = read
        .text
        .split("#")
        .nth(1)
        .and_then(|rest| rest.get(..4))
        .expect("read header carries a tag")
        .to_owned();

    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar($A)", "dry_run": false}),
    );
    assert!(!out.is_error, "{}", out.text);
    let patch = format!("[a.rs#{tag}]\nPUT 2.=2:\n+fn g() {{ /* changed */ }}\n");
    let stale = execute(&ws, "edit", &json!({"input": patch}), 20);
    assert!(
        stale.is_error && stale.text.contains("unknown snapshot tag"),
        "{}",
        stale.text
    );

    // Reading again yields a fresh tag that works.
    let reread = execute(&ws, "read", &json!({"path": "a.rs"}), 20);
    assert!(reread.text.contains("bar(1)"), "{}", reread.text);
    let fresh = reread
        .text
        .split("#")
        .nth(1)
        .and_then(|rest| rest.get(..4))
        .unwrap()
        .to_owned();
    let patch = format!("[a.rs#{fresh}]\nPUT 2.=2:\n+fn g() {{ /* changed */ }}\n");
    let ok = execute(&ws, "edit", &json!({"input": patch}), 20);
    assert!(!ok.is_error, "{}", ok.text);
}

#[cfg(unix)]
#[test]
fn permissions_and_crlf_line_endings_are_preserved() {
    let (dir, ws) = workspace();
    let script = put(&dir, "run.py", "import os\r\nfoo(1)\r\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar(\n    $A,\n)", "dry_run": false}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        fs::read(&script).unwrap(),
        b"import os\r\nbar(\r\n    1,\r\n)\r\n"
    );
    assert_eq!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o777,
        0o750
    );
}

#[test]
fn multi_line_rewrites_show_hunks_with_shifted_line_numbers() {
    let (dir, ws) = workspace();
    put(&dir, "a.py", "a = 1\nfoo(1)\nb = 2\nc = 3\nfoo(2)\n");
    let out = edit(
        &ws,
        json!({"pattern": "foo($A)", "rewrite": "bar(\n    $A,\n)"}),
    );
    assert!(
        out.text
            .contains("@@ -2,1 +2,3 @@\n-foo(1)\n+bar(\n+    1,\n+)\n"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("@@ -5,1 +7,3 @@\n-foo(2)\n"),
        "{}",
        out.text
    );
}

#[test]
fn no_matches_is_not_an_error_and_every_listed_language_parses() {
    let (dir, ws) = workspace();
    put(&dir, "a.rs", "fn f() {}\n");
    let out = edit(
        &ws,
        json!({"pattern": "absent($A)", "rewrite": "x", "dry_run": false}),
    );
    assert!(
        !out.is_error && out.text.starts_with("no matches"),
        "{}",
        out.text
    );
    for name in lang_names().split(", ") {
        assert!(engine::parse_lang(name).is_ok(), "{name}");
    }
}

#[test]
fn schemas_follow_the_tool_contract() {
    for tool in tools(&Config::default()) {
        let schema = tool.schema();
        assert_eq!(schema["function"]["name"], tool.name());
        assert!(
            schema["function"]["description"]
                .as_str()
                .unwrap()
                .contains("rust")
        );
    }
}

fn token_of(preview: &str) -> String {
    preview
        .lines()
        .find_map(|line| line.strip_prefix("apply_token: "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("no apply_token in:\n{preview}"))
        .to_owned()
}

#[test]
fn apply_token_applies_exactly_the_previewed_rewrite() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.rs", "fn main() {\n    old(1);\n}\n");
    let b = put(&dir, "b.rs", "fn other() { old(2); }\n");
    let preview = edit(&ws, json!({"pattern": "old($A)", "rewrite": "new($A)"}));
    assert!(!preview.is_error, "{}", preview.text);
    let token = token_of(&preview.text);
    assert!(fs::read_to_string(&a).unwrap().contains("old(1)"));
    assert!(AstEdit.needs_approval(&json!({"apply_token": token})));

    let done = edit(&ws, json!({"apply_token": token}));
    assert!(!done.is_error, "{}", done.text);
    assert!(
        done.text.contains("applied 2 replacements in 2 files"),
        "{}",
        done.text
    );
    assert_eq!(
        fs::read_to_string(&a).unwrap(),
        "fn main() {\n    new(1);\n}\n"
    );
    assert_eq!(fs::read_to_string(&b).unwrap(), "fn other() { new(2); }\n");

    // A token is single-use.
    let again = edit(&ws, json!({"apply_token": token}));
    assert!(
        again.is_error && again.text.contains("unknown or expired"),
        "{}",
        again.text
    );
}

#[test]
fn apply_token_is_refused_when_an_affected_file_changed_since_the_preview() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.rs", "fn f() { old(1); }\n");
    let b = put(&dir, "b.rs", "fn g() { old(2); }\n");
    let token = token_of(&edit(&ws, json!({"pattern": "old($A)", "rewrite": "new($A)"})).text);
    fs::write(&b, "fn g() { old(2); extra(); }\n").unwrap();

    let refused = edit(&ws, json!({"apply_token": token}));
    assert!(refused.is_error, "{}", refused.text);
    assert!(
        refused.text.contains("changed after AST planning"),
        "{}",
        refused.text
    );
    assert_eq!(
        fs::read_to_string(&a).unwrap(),
        "fn f() { old(1); }\n",
        "all-or-nothing"
    );
    assert_eq!(
        fs::read_to_string(&b).unwrap(),
        "fn g() { old(2); extra(); }\n"
    );
}

#[test]
fn apply_token_rejects_unknown_tokens_and_mixed_arguments() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.rs", "fn f() { old(1); }\n");
    let unknown = edit(&ws, json!({"apply_token": "deadbeef"}));
    assert!(unknown.is_error && unknown.text.contains("unknown or expired"));

    let token = token_of(&edit(&ws, json!({"pattern": "old($A)", "rewrite": "new($A)"})).text);
    for extra in [
        json!({"pattern": "x"}),
        json!({"path": "a.rs"}),
        json!({"dry_run": true}),
    ] {
        let mut args = json!({"apply_token": token});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let out = edit(&ws, args);
        assert!(out.is_error, "{}", out.text);
    }
    assert!(
        fs::read_to_string(&a).unwrap().contains("old(1)"),
        "mixed calls write nothing"
    );
    // The rejected calls did not spend the token.
    assert!(!edit(&ws, json!({"apply_token": token})).is_error);
}

#[test]
fn staged_previews_are_bounded_shared_by_clones_and_skipped_for_refusals() {
    let (dir, ws) = workspace();
    put(&dir, "a.rs", "fn f() { old(1); }\n");
    let clone = ws.clone();
    let tokens: Vec<String> = (0..6)
        .map(|n| {
            token_of(
                &edit(
                    &ws,
                    json!({"pattern": "old($A)", "rewrite": format!("new{n}($A)")}),
                )
                .text,
            )
        })
        .collect();
    // The oldest previews were evicted; the newest applies through a clone of the workspace.
    let evicted = edit(&clone, json!({"apply_token": tokens[0]}));
    assert!(
        evicted.is_error && evicted.text.contains("unknown or expired"),
        "{}",
        evicted.text
    );
    let applied = edit(&clone, json!({"apply_token": tokens[5]}));
    assert!(!applied.is_error, "{}", applied.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn f() { new5(1); }\n"
    );

    let none = edit(&ws, json!({"pattern": "absent($A)", "rewrite": "x($A)"}));
    assert!(!none.text.contains("apply_token"), "{}", none.text);
}

#[test]
fn full_dry_run_false_call_still_applies_and_old_schema_history_stays_valid() {
    let (dir, ws) = workspace();
    let a = put(&dir, "a.rs", "fn f() { old(1); }\n");
    let done = edit(
        &ws,
        json!({"pattern": "old($A)", "rewrite": "new($A)", "dry_run": false}),
    );
    assert!(!done.is_error, "{}", done.text);
    assert_eq!(fs::read_to_string(&a).unwrap(), "fn f() { new(1); }\n");

    let registry = crate::tools::integrations::ext::Registry::new();
    let old =
        crate::tools::surface::contracts::identities(&[edit_schema(false)], &registry)["ast_edit"]
            .clone();
    assert!(AstEdit.accepts_historical_contract(&old));
    let current =
        crate::tools::surface::contracts::identities(&[AstEdit.schema()], &registry)["ast_edit"]
            .clone();
    assert_ne!(old, current, "the token property is a real schema change");
}

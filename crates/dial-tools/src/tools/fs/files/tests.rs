use super::*;
use crate::tools::testkit::*;
use serde_json::json;
#[test]
fn directory_listing_is_by_name_and_does_not_expand_generated_directories() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join(".gitignore"), "/build/\n").unwrap();
    fs::write(dir.path().join("b.txt"), "").unwrap();
    fs::write(dir.path().join("a.txt"), "").unwrap();
    for name in ["src", "target", "build", ".git", "node_modules"] {
        fs::create_dir_all(dir.path().join(name)).unwrap();
        fs::write(dir.path().join(name).join("inner.rs"), "").unwrap();
    }
    let worktree = dir.path().join("copy");
    fs::create_dir_all(&worktree).unwrap();
    fs::write(worktree.join(".git"), "gitdir: /r/.git/worktrees/copy\n").unwrap();
    fs::write(worktree.join("inner.rs"), "").unwrap();
    let out = run(&ws, "read", json!({"path": "."})).text;
    let listing: Vec<_> = out.lines().skip(1).collect();
    assert_eq!(
        listing,
        [
            ".git/ [ignored/generated; not expanded]",
            "build/ [ignored/generated; not expanded]",
            "copy/ [ignored/generated; not expanded]",
            "node_modules/ [ignored/generated; not expanded]",
            "src/",
            "  inner.rs",
            "target/ [ignored/generated; not expanded]",
            ".gitignore",
            "a.txt",
            "b.txt",
        ],
        "{out}"
    );
}
#[test]
fn read_numbers_lines_and_pages() {
    let (dir, ws) = workspace();
    let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    fs::write(dir.path().join("f.txt"), &body).unwrap();
    let out = run(
        &ws,
        "read",
        json!({"path": "f.txt", "offset": 3, "limit": 2}),
    );
    assert!(!out.is_error);
    assert!(
        out.text.contains("     3\tline 3\n     4\tline 4\n"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("line 5\n"));
    assert!(
        out.text
            .ends_with("[lines 3-4 of 10; continue with `f.txt:5-`]\n"),
        "{}",
        out.text
    );
    let past = run(&ws, "read", json!({"path": "f.txt", "offset": 99}));
    assert!(
        past.text
            .contains("no lines in that range; file has 10 lines"),
        "{}",
        past.text
    );
    assert!(run(&ws, "read", json!({"path": "f.txt", "offset": 0})).is_error);
    fs::write(dir.path().join("empty"), "").unwrap();
    assert_eq!(
        run(&ws, "read", json!({"path": "empty"})).text,
        "(empty file)"
    );
    fs::write(dir.path().join("bin"), b"abc\0def").unwrap();
    assert!(
        run(&ws, "read", json!({"path": "bin"}))
            .text
            .contains("binary")
    );
    let missing = run(&ws, "read", json!({"path": "missing"}));
    assert!(missing.is_error && missing.text.contains("no such file"));
    let listing = run(&ws, "read", json!({"path": "."}));
    assert!(listing.text.contains("f.txt") && listing.text.contains("directory"));
}

#[test]
fn read_output_is_capped_at_64_kib() {
    let (dir, ws) = workspace();
    let line = "x".repeat(1000);
    let body: String = (0..500).map(|_| format!("{line}\n")).collect();
    fs::write(dir.path().join("big.txt"), &body).unwrap();
    let out = run(&ws, "read", json!({"path": "big.txt", "limit": 10000}));
    assert!(!out.is_error);
    assert!(
        out.text.len() <= MAX_READ_OUTPUT + 100,
        "{}",
        out.text.len()
    );
    assert!(
        out.text.contains("output truncated") && out.text.contains("continue with `big.txt:"),
        "{}",
        out.text
    );
    // A single enormous line is truncated, not buffered.
    fs::write(dir.path().join("long.txt"), "y".repeat(5_000_000)).unwrap();
    let long = run(&ws, "read", json!({"path": "long.txt"}));
    assert!(long.text.len() < 4000 && long.text.contains("line truncated"));
}

#[test]
fn read_budget_bounds_the_rows_and_the_multi_path_total() {
    let (dir, ws) = workspace();
    let body: String = (1..=2000)
        .map(|n| format!("line {n} with some padding text\n"))
        .collect();
    fs::write(dir.path().join("long.txt"), &body).unwrap();
    fs::write(dir.path().join("short.txt"), "alpha\nbeta\n").unwrap();
    fs::write(dir.path().join("other.txt"), &body).unwrap();
    let budget = 4096;
    let single = read_tool_streaming(
        &ws,
        &json!({"path": "long.txt"}),
        &std::sync::atomic::AtomicBool::new(false),
        budget,
    )
    .unwrap();
    assert!(single.len() <= budget, "{}", single.len());
    assert!(single.contains("of 2000"), "{single}");
    assert!(!single.contains("line 2000 "), "{single}");
    let several = read_tool_streaming(
        &ws,
        &json!({"paths": ["short.txt", "other.txt"]}),
        &std::sync::atomic::AtomicBool::new(false),
        budget,
    )
    .unwrap();
    assert!(
        several.len() <= budget + READ_FOOTER_RESERVE,
        "{}",
        several.len()
    );
    assert!(
        several.contains("alpha") && several.contains("other.txt"),
        "{several}"
    );
}

fn numbered_file(dir: &tempfile::TempDir, name: &str, lines: usize) {
    let body: String = (1..=lines).map(|i| format!("line {i}\n")).collect();
    fs::write(dir.path().join(name), body).unwrap();
}

#[test]
fn read_accepts_path_selectors_for_ranges_tail_and_multiple_ranges() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "f.txt", 20);
    let read = |path: &str| run(&ws, "read", json!({ "path": path }));
    assert_eq!(
        read("f.txt:3-4").text,
        "     3\tline 3\n     4\tline 4\n[lines 3-4 of 20]\n"
    );
    assert_eq!(read("f.txt:3+2").text, read("f.txt:3-4").text);
    let tail = read("f.txt:-2").text;
    assert_eq!(
        tail,
        "    19\tline 19\n    20\tline 20\n[lines 19-20 of 20]\n"
    );
    let multi = read("f.txt:2-3,18-").text;
    assert_eq!(
        multi,
        "     2\tline 2\n     3\tline 3\n…\n    18\tline 18\n    19\tline 19\n    20\tline 20\n[lines 2-3, 18-20 of 20]\n"
    );
    assert_eq!(read("f.txt:raw:19-20").text, "line 19\nline 20\n");
    assert!(read("f.txt:0").is_error);
    assert!(read("f.txt:9-3").is_error);
    assert!(
        read("f.txt:1-20")
            .text
            .ends_with("    20\tline 20\n[lines 1-20 of 20]\n")
    );
}

#[test]
fn read_ranges_clip_at_the_end_of_the_file() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "f.txt", 5);
    let read = |path: &str| run(&ws, "read", json!({ "path": path })).text;
    assert_eq!(
        read("f.txt:4-99"),
        "     4\tline 4\n     5\tline 5\n[lines 4-5 of 5]\n"
    );
    assert!(read("f.txt:6-9").contains("no lines in that range; file has 5 lines"));
    // A range past the end is skipped; the valid one is still returned.
    assert_eq!(read("f.txt:2-2,50-60"), "     2\tline 2\n[lines 2 of 5]\n");
    assert!(read("f.txt:-100").contains("[lines 1-5 of 5]"));
    // Open-ended ranges are bounded by `limit` and say how to continue.
    let paged = run(&ws, "read", json!({"path": "f.txt:2-", "limit": 2})).text;
    assert!(
        paged.ends_with("[lines 2-3 of 5; continue with `f.txt:4-`]\n"),
        "{paged}"
    );
    // An empty file has no lines for any selector.
    fs::write(dir.path().join("empty"), "").unwrap();
    assert_eq!(read("empty:3-5"), "(empty file)");
    assert_eq!(read("empty:-3"), "(empty file)");
}

#[test]
fn read_without_trailing_newline_counts_the_last_line() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("f.txt"), "a\nb\nc").unwrap();
    let out = run(&ws, "read", json!({"path": "f.txt:-1"})).text;
    assert_eq!(out, "     3\tc\n[lines 3 of 3]\n");
    let head = run(&ws, "read", json!({"path": "f.txt", "limit": 1})).text;
    assert!(
        head.ends_with("[lines 1 of 3; continue with `f.txt:2-`]\n"),
        "{head}"
    );
}

#[test]
fn read_rejects_offset_combined_with_a_selector_and_reads_odd_names_literally() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "f.txt", 5);
    let both = run(&ws, "read", json!({"path": "f.txt:2-3", "offset": 2}));
    assert!(
        both.is_error && both.text.contains("not both"),
        "{}",
        both.text
    );
    fs::write(dir.path().join("weird:5"), "x\n").unwrap();
    assert_eq!(
        run(&ws, "read", json!({"path": "weird:5"})).text,
        "     1\tx\n"
    );
}

#[test]
fn write_creates_parents_and_enforces_limit() {
    let (dir, ws) = workspace();
    let out = run(&ws, "write", json!({"path": "a/b/c.txt", "content": "hi"}));
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
        "hi"
    );
    assert!(
        run(
            &ws,
            "write",
            json!({"path": "a/b/c.txt", "content": "again"})
        )
        .text
        .contains("overwrote")
    );
    let big = "z".repeat(MAX_WRITE + 1);
    let refused = run(&ws, "write", json!({"path": "big", "content": big}));
    assert!(refused.is_error && !dir.path().join("big").exists());
    let exact = "z".repeat(MAX_WRITE);
    assert!(!run(&ws, "write", json!({"path": "big", "content": exact})).is_error);
    assert!(run(&ws, "write", json!({"path": "a", "content": "x"})).is_error);
    let selector = run(
        &ws,
        "write",
        json!({"path": "literal.rs:10-20", "content": "x"}),
    );
    assert!(selector.is_error && !dir.path().join("literal.rs:10-20").exists());
}

#[test]
fn refused_unread_overwrite_shows_small_files_as_a_witness_for_the_next_edit() {
    let (dir, ws) = workspace();
    let ws = ws.with_edit_mode(crate::tools::EditMode::Hashline);
    fs::write(dir.path().join("README.md"), "# Old\n").unwrap();
    let refused = run(
        &ws,
        "write",
        json!({"path": "README.md", "content": "# New\n"}),
    );
    assert!(refused.is_error && refused.text.contains("no current source/read witness"));
    assert!(
        refused.text.contains("Nothing was written") && refused.text.contains("\n1:# Old\n"),
        "{}",
        refused.text
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("README.md")).unwrap(),
        "# Old\n"
    );
    let header = refused
        .text
        .lines()
        .find(|line| line.starts_with("[README.md#"))
        .unwrap();
    let edited = run(
        &ws,
        "edit",
        json!({"input": format!("{header}\nPUT 1.=1:\n+# New\n")}),
    );
    assert!(!edited.is_error, "{}", edited.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("README.md")).unwrap(),
        "# New\n"
    );

    // A file too large to show whole keeps the plain refusal and gains no witness.
    let large: String = (0..201).map(|n| format!("line {n}\n")).collect();
    fs::write(dir.path().join("large.txt"), &large).unwrap();
    let refused = run(&ws, "write", json!({"path": "large.txt", "content": "x\n"}));
    assert!(
        refused.is_error && !refused.text.contains("Nothing was written"),
        "{}",
        refused.text
    );
    // The real current tag still has no witness, so an edit against it is refused too.
    let tag = crate::tools::edit::hashline::tag_of(&large);
    let edit = run(
        &ws,
        "edit",
        json!({"input": format!("[large.txt#{tag}]\nPUT 1.=1:\n+x\n")}),
    );
    assert!(edit.is_error, "{}", edit.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("large.txt")).unwrap(),
        large
    );
}

#[test]
fn overwrite_recovery_witness_binds_the_snapshot_not_a_second_disk_read() {
    let (dir, ws) = workspace();
    let ws = ws.with_edit_mode(crate::tools::EditMode::Hashline);
    let path = dir.path().join("file.txt");
    let before = "original\n";
    let now = "new\n".repeat(201);
    fs::write(&path, &now).unwrap();
    let shown = unseen_overwrite_projection(&ws, &path, before).unwrap();
    assert!(shown.contains("\n1:original\n") && !shown.contains("\n1:new\n"));
    assert!(crate::tools::edit::editing::seen_guard(&ws, &path, before, &[(1, 1)]).is_ok());
    assert!(crate::tools::edit::editing::seen_guard(&ws, &path, &now, &[(1, 201)]).is_err());
}

#[test]
fn edit_requires_unique_match_unless_replace_all() {
    let (dir, ws) = workspace();
    let file = dir.path().join("e.txt");
    fs::write(&file, "foo bar foo\n").unwrap();
    run(&ws, "read", json!({"path":"e.txt"}));
    let ambiguous = run(
        &ws,
        "edit",
        json!({"path": "e.txt", "old_string": "foo", "new_string": "baz"}),
    );
    assert!(
        ambiguous.is_error && ambiguous.text.contains("matches 2 places"),
        "{}",
        ambiguous.text
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "foo bar foo\n");
    let missing = run(
        &ws,
        "edit",
        json!({"path": "e.txt", "old_string": "nope", "new_string": "x"}),
    );
    assert!(missing.is_error && missing.text.contains("not found"));
    let unique = run(
        &ws,
        "edit",
        json!({"path": "e.txt", "old_string": "bar", "new_string": "BAR"}),
    );
    assert!(!unique.is_error);
    assert_eq!(fs::read_to_string(&file).unwrap(), "foo BAR foo\n");
    let all = run(
        &ws,
        "edit",
        json!({"path": "e.txt", "old_string": "foo", "new_string": "baz", "replace_all": true}),
    );
    assert!(all.text.contains("2 occurrences"));
    assert_eq!(fs::read_to_string(&file).unwrap(), "baz BAR baz\n");
    assert!(
        run(
            &ws,
            "edit",
            json!({"path": "e.txt", "old_string": "", "new_string": "x"})
        )
        .is_error
    );
    assert!(
        run(
            &ws,
            "edit",
            json!({"path": "e.txt", "old_string": "baz", "new_string": "baz"})
        )
        .is_error
    );
}

fn edit_err(ws: &Workspace, path: &str, old: &str) -> String {
    let out = run(
        ws,
        "edit",
        json!({"path": path, "old_string": old, "new_string": "REPLACED"}),
    );
    assert!(out.is_error, "{}", out.text);
    out.text
}

#[test]
fn a_whitespace_only_slip_is_applied_and_reported_when_the_match_is_unique_and_uniform() {
    let (dir, ws) = workspace();
    let file = dir.path().join("a.py");
    fs::write(&file, "def f():\n    x = 1\n    return x\n").unwrap();
    run(&ws, "read", json!({"path":"a.py"}));
    // The model dropped two spaces of indentation from every line of the block.
    let out = run(
        &ws,
        "edit",
        json!({"path": "a.py", "old_string": "  x = 1\n  return x", "new_string": "  x = 2\n  return x"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.contains("only in whitespace")
            && out.text.contains("lines 2-3")
            && out.text.contains("2 spaces more"),
        "{}",
        out.text
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "def f():\n    x = 2\n    return x\n"
    );
}

#[test]
fn an_ambiguous_whitespace_slip_and_a_disabled_fuzzy_match_are_refused() {
    let (dir, ws) = workspace();
    let file = dir.path().join("a.py");
    let original = "def f():\n    x = 1\n    return x\n\ndef g():\n    x = 1\n    return x\n";
    fs::write(&file, original).unwrap();
    let err = edit_err(&ws, "a.py", "x = 1\nreturn x");
    assert!(
        err.contains("matches 2 places") && err.contains("lines 2, 6"),
        "{err}"
    );
    fs::write(&file, "def f():\n    x = 1\n    return x\n").unwrap();
    // The canonical settings now allow native normalized matching by default. Exact-only
    // operators can disable fuzzy recovery; a non-exact indentation copy must then fail.
    let mut settings = ws.edit_settings().clone();
    settings.mode = crate::tools::EditMode::Replace;
    settings.fuzzy_match = false;
    let exact = ws.clone().with_edit_settings(settings);
    let err = edit_err(&exact, "a.py", "def f():\n  x = 1\n return x");
    assert!(
        err.contains("lines 1-3") && err.contains("whitespace") && err.contains("2:    x = 1"),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "def f():\n    x = 1\n    return x\n"
    );
}

#[test]
fn a_missed_edit_that_copied_read_line_numbers_says_so() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    let err = edit_err(&ws, "a.txt", "     1\talpha\n     2\tbeta");
    assert!(
        err.contains("line number") && err.contains("after the tab"),
        "{err}"
    );
}

#[test]
fn a_drifted_block_points_at_where_its_first_line_is() {
    let (dir, ws) = workspace();
    fs::write(
        dir.path().join("a.rs"),
        "fn a() {}\nfn target() {\n    new_body();\n}\n",
    )
    .unwrap();
    let err = edit_err(&ws, "a.rs", "fn target() {\n    old_body();\n}");
    assert!(
        err.contains("first line occurs") && err.contains("2:fn target() {"),
        "{err}"
    );
    let gone = edit_err(&ws, "a.rs", "fn missing_entirely() {}");
    assert!(gone.contains("Not even its first line"), "{gone}");
}

#[test]
fn an_ambiguous_edit_lists_where_the_matches_are() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "x = 1\nfoo\nbar\nfoo\n").unwrap();
    let err = edit_err(&ws, "a.txt", "foo");
    assert!(
        err.contains("matches 2 places") && err.contains("lines 2, 4"),
        "{err}"
    );
}

#[test]
fn a_crlf_file_is_edited_with_lf_text_and_stays_crlf() {
    let (dir, ws) = workspace();
    let file = dir.path().join("w.txt");
    fs::write(&file, "one\r\ntwo\r\nthree\r\n").unwrap();
    run(&ws, "read", json!({"path":"w.txt"}));
    let out = run(
        &ws,
        "edit",
        json!({"path": "w.txt", "old_string": "one\ntwo", "new_string": "1\n2\n2b"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "1\r\n2\r\n2b\r\nthree\r\n"
    );
}

use super::*;
use crate::tools::testkit::*;
use serde_json::json;
#[cfg(unix)]
use std::os::unix::fs::symlink;
fn body(text: &str) -> String {
    let body = text.split("[coverage:").next().unwrap_or(text);
    let filtered: Vec<_> = body
        .lines()
        .filter(|line| {
            let Some((path, tag)) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
                .and_then(|line| line.rsplit_once('#'))
            else {
                return true;
            };
            path.is_empty() || tag.len() != 4 || !tag.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .collect();
    let mut result = filtered.join("\n");
    if body.ends_with('\n') {
        result.push('\n');
    }
    result
}

#[test]
fn nested_linked_worktrees_are_skipped_but_submodules_are_not() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("main.rs"), "needle\n").unwrap();
    let copy = dir.path().join(".claude/worktrees/feature");
    fs::create_dir_all(&copy).unwrap();
    fs::write(copy.join(".git"), "gitdir: /repo/.git/worktrees/feature\n").unwrap();
    fs::write(copy.join("main.rs"), "needle\n").unwrap();
    let module = dir.path().join("vendor/lib");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join(".git"), "gitdir: ../../.git/modules/lib\n").unwrap();
    fs::write(module.join("lib.rs"), "needle\n").unwrap();

    let glob = run(&ws, "glob", json!({"pattern": "**/*.rs"})).text;
    assert_eq!(body(&glob), "main.rs\nvendor/lib/lib.rs\n", "{glob}");
    let grep = grep(&ws, json!({"pattern": "needle"}));
    assert!(!grep.contains(".claude/worktrees"), "{grep}");
    assert!(grep.contains("vendor/lib/lib.rs"), "{grep}");
}

#[test]
fn complete_walks_report_a_short_footer_and_limited_walks_keep_details() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
    let text = grep(&ws, json!({"pattern": "needle"}));
    let footer = text.split("[coverage:").nth(1).unwrap();
    assert!(footer.contains("walk complete"), "{text}");
    assert!(!footer.contains("directory_entry_limit"), "{text}");
    assert!(footer.contains(r#""next_cursor":null"#), "{text}");
    assert!(!footer.contains("order"), "{text}");
    for i in 0..3 {
        fs::write(dir.path().join(format!("m{i}.txt")), "needle\n").unwrap();
    }
    let capped = grep(&ws, json!({"pattern": "needle", "max_results": 1}));
    let footer = capped.split("[coverage:").nth(1).unwrap();
    assert!(
        footer.contains("omitted coverage is not exhaustive"),
        "{capped}"
    );
    assert!(footer.contains(r#""order""#), "{capped}");
}
#[cfg(unix)]
#[test]
fn glob_and_grep_do_not_follow_symlinks() {
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("leak.txt"), "needle outside\n").unwrap();
    symlink(outside.path(), dir.path().join("linkdir")).unwrap();
    symlink(
        outside.path().join("leak.txt"),
        dir.path().join("linkfile.txt"),
    )
    .unwrap();
    fs::write(dir.path().join("real.txt"), "needle inside\n").unwrap();

    let glob = run(&ws, "glob", json!({"pattern": "**/*.txt"}));
    assert_eq!(body(&glob.text).trim(), "real.txt");
    let grep = run(&ws, "grep", json!({"pattern": "needle"}));
    assert!(grep.text.contains("real.txt:1:needle inside"));
    assert!(!grep.text.contains("outside"));
    let direct = run(&ws, "grep", json!({"pattern": "needle", "path": "linkdir"}));
    assert!(direct.is_error);
}

#[test]
fn glob_patterns_sort_and_cap() {
    let (dir, ws) = workspace();
    for path in [
        "src/b.rs",
        "src/a.rs",
        "src/deep/c.rs",
        "README.md",
        "docs/x.md",
        ".git/config.rs",
        ".tmp/t.rs",
    ] {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, "").unwrap();
    }
    let text = |p: &str| {
        let result = run(&ws, "glob", json!({"pattern": p})).text;
        let rows = body(&result);
        if rows == "no matches\n" {
            "no matches".to_string()
        } else {
            rows.to_string()
        }
    };
    assert_eq!(text("src/*.rs"), "src/a.rs\nsrc/b.rs\n");
    assert_eq!(text("src/**/*.rs"), "src/a.rs\nsrc/b.rs\nsrc/deep/c.rs\n");
    assert_eq!(text("*.rs"), "src/a.rs\nsrc/b.rs\nsrc/deep/c.rs\n");
    assert_eq!(
        text("*.{md,rs}"),
        "README.md\ndocs/x.md\nsrc/a.rs\nsrc/b.rs\nsrc/deep/c.rs\n"
    );
    assert_eq!(text("src/[ab].rs"), "src/a.rs\nsrc/b.rs\n");
    assert_eq!(text("src/?.rs"), "src/a.rs\nsrc/b.rs\n");
    assert_eq!(text("docs/"), "docs/x.md\n");
    assert_eq!(text("nothing*"), "no matches");
    assert!(run(&ws, "glob", json!({"pattern": "../*"})).is_error);
    assert!(run(&ws, "glob", json!({"pattern": "/etc/*"})).is_error);
    for i in 0..(MAX_LIST + 20) {
        fs::write(dir.path().join(format!("many_{i:04}.dat")), "").unwrap();
    }
    let many = run(&ws, "glob", json!({"pattern": "many_*.dat"})).text;
    assert_eq!(body(&many).lines().count(), MAX_LIST);
    let footer = many
        .lines()
        .find_map(|line| {
            line.strip_prefix("[page: ")
                .and_then(|s| s.strip_suffix(']'))
        })
        .unwrap();
    let page: Value = serde_json::from_str(footer).unwrap();
    assert_eq!(
        page["next_cursor"],
        json!({"path": "many_0500.dat", "match": 0})
    );
}

fn grep(ws: &Workspace, args: serde_json::Value) -> String {
    run(ws, "grep", args).text
}

fn numbered_file(dir: &tempfile::TempDir, name: &str, lines: usize) {
    let body: String = (1..=lines).map(|i| format!("l{i}\n")).collect();
    fs::write(dir.path().join(name), body).unwrap();
}

#[test]
fn grep_fixed_string_treats_metacharacters_literally() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "Hello World\nfn a(x) {}\nax\n").unwrap();
    let literal = grep(&ws, json!({"pattern": "a(x)", "fixed_string": true}));
    assert!(
        literal.contains("a.txt:2:fn a(x) {}") && !literal.contains("a.txt:3:"),
        "{literal}"
    );
    assert!(grep(&ws, json!({"pattern": ".*", "fixed_string": true})).starts_with("no matches\n"));
    let folded = grep(
        &ws,
        json!({"pattern": "HELLO", "fixed_string": true, "ignore_case": true}),
    );
    assert!(body(&folded).starts_with("a.txt:1:Hello World"), "{folded}");
}

#[test]
fn grep_is_a_case_sensitive_regex_by_default() {
    let (dir, ws) = workspace();
    fs::write(
        dir.path().join("a.rs"),
        "fn alpha(x: u8) {}\nlet fn_ptr = 1;\nHELLO\nhello\n",
    )
    .unwrap();
    let calls = grep(&ws, json!({"pattern": r"^fn \w+\("}));
    assert!(
        calls.contains("a.rs:1:fn alpha(x: u8)") && !calls.contains("a.rs:2:"),
        "{calls}"
    );
    let alternation = grep(&ws, json!({"pattern": "alpha|fn_ptr"}));
    assert!(
        alternation.contains("a.rs:1:") && alternation.contains("a.rs:2:"),
        "{alternation}"
    );
    // `a(x)` is a group here: it matches `ax`, not the literal text.
    assert!(grep(&ws, json!({"pattern": "a(l)pha"})).contains("a.rs:1:"));
    assert!(grep(&ws, json!({"pattern": "a(x)"})).starts_with("no matches\n"));
    let sensitive = grep(&ws, json!({"pattern": "hello"}));
    assert!(
        sensitive.contains("a.rs:4:hello") && !sensitive.contains("a.rs:3:"),
        "{sensitive}"
    );
    let folded = grep(&ws, json!({"pattern": "^hello$", "ignore_case": true}));
    assert!(
        folded.contains("a.rs:3:HELLO") && folded.contains("a.rs:4:hello"),
        "{folded}"
    );
}

#[test]
fn grep_reports_invalid_regex_without_panicking() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "x(\n").unwrap();
    let bad = run(&ws, "grep", json!({"pattern": "x("}));
    assert!(bad.is_error, "{}", bad.text);
    // The same text is fine as a literal.
    assert!(grep(&ws, json!({"pattern": "x(", "fixed_string": true})).contains("a.txt:1:x("));
    let huge = run(&ws, "grep", json!({"pattern": "((a{100}){100}){100}"}));
    assert!(huge.is_error, "{}", huge.text);
    assert!(run(&ws, "grep", json!({"pattern": ""})).is_error);
    assert!(run(&ws, "grep", json!({"pattern": "x", "max_results": 0})).is_error);
}

#[test]
fn grep_filters_by_glob_and_skips_binary_files() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "Hello World\n").unwrap();
    fs::write(dir.path().join("b.md"), "hello again\n").unwrap();
    fs::write(dir.path().join("bin.dat"), b"hello\0").unwrap();
    let all = grep(&ws, json!({"pattern": "hello"}));
    assert!(
        all.contains("[b.md#")
            && all.contains("b.md:1:hello again")
            && !all.contains("a.txt")
            && all.contains("binary_files=1"),
        "{all}"
    );
    let only_txt = grep(&ws, json!({"pattern": "(?i)hello", "glob": "*.txt"}));
    assert!(
        only_txt.contains("[a.txt#")
            && only_txt.contains("a.txt:1:Hello World")
            && !only_txt.contains("b.md"),
        "{only_txt}"
    );
    let single = grep(&ws, json!({"pattern": "again", "path": "b.md"}));
    assert!(single.contains("b.md:1:hello again"));
}

#[test]
fn grep_context_lines_merge_and_separate_groups() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "f.txt", 10);
    let one = grep(&ws, json!({"pattern": "l5$", "context": 1}));
    assert_eq!(body(&one), "f.txt-4-l4\nf.txt:5:l5\nf.txt-6-l6\n");
    // Distant matches are split by `--`; overlapping windows merge.
    let apart = grep(&ws, json!({"pattern": "l[27]$", "context": 1}));
    assert_eq!(
        body(&apart),
        "f.txt-1-l1\nf.txt:2:l2\nf.txt-3-l3\n--\nf.txt-6-l6\nf.txt:7:l7\nf.txt-8-l8\n"
    );
    let merged = grep(&ws, json!({"pattern": "l[24]$", "context": 1}));
    assert_eq!(
        body(&merged),
        "f.txt-1-l1\nf.txt:2:l2\nf.txt-3-l3\nf.txt:4:l4\nf.txt-5-l5\n"
    );
    // Windows are clipped at both ends of the file.
    assert_eq!(
        body(&grep(&ws, json!({"pattern": "l1$", "context": 2}))),
        "f.txt:1:l1\nf.txt-2-l2\nf.txt-3-l3\n"
    );
    assert_eq!(
        body(&grep(&ws, json!({"pattern": "l10$", "context": 2}))),
        "f.txt-8-l8\nf.txt-9-l9\nf.txt:10:l10\n"
    );
    // No context means plain match rows with no separators.
    assert_eq!(
        body(&grep(&ws, json!({"pattern": "l[27]$"}))),
        "f.txt:2:l2\nf.txt:7:l7\n"
    );
}

#[test]
fn grep_context_separates_files_and_counts_only_matches() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "a.txt", 3);
    numbered_file(&dir, "b.txt", 3);
    let two = grep(&ws, json!({"pattern": "l2$", "context": 1}));
    assert_eq!(
        body(&two),
        "a.txt-1-l1\na.txt:2:l2\na.txt-3-l3\n--\nb.txt-1-l1\nb.txt:2:l2\nb.txt-3-l3\n"
    );
    // max_results limits matches, not the context rows around them.
    let one = grep(
        &ws,
        json!({"pattern": "l2$", "context": 1, "max_results": 1}),
    );
    assert!(
        body(&one).starts_with("a.txt-1-l1\na.txt:2:l2\na.txt-3-l3\n"),
        "{one}"
    );
    assert!(
        !body(&one).contains("b.txt")
            && one.contains("results=1")
            && one.contains("\"next_cursor\":{"),
        "{one}"
    );
}

#[test]
fn grep_caps_matches_and_bounds_each_line() {
    let (dir, ws) = workspace();
    let many: String = (0..1000).map(|_| "hit\n").collect();
    fs::write(dir.path().join("many.log"), many).unwrap();
    // Without max_results, one file contributes at most GREP_PER_FILE matches to the page; the
    // rest of it is named in the footer, and the walk goes on to later files.
    let capped = grep(&ws, json!({"pattern": "hit", "path": "many.log"}));
    assert_eq!(capped.matches("many.log:").count(), GREP_PER_FILE);
    assert!(
        capped.contains(&format!(
            "per_file_cap={GREP_PER_FILE} reached in: many.log"
        )),
        "{capped}"
    );
    assert!(capped.contains("\"next_cursor\":null"), "{capped}");
    let five = grep(
        &ws,
        json!({"pattern": "hit", "path": "many.log", "max_results": 5}),
    );
    assert_eq!(five.matches("many.log:").count(), 5);
    // An explicit max_results lifts the per-file cap; an oversized one is clamped, not refused.
    let raised = grep(
        &ws,
        json!({"pattern": "hit", "path": "many.log", "max_results": 5000}),
    );
    assert_eq!(
        raised.matches("many.log:").count(),
        MAX_GREP_MATCHES,
        "an oversized max_results is clamped, not refused"
    );
    fs::write(
        dir.path().join("long.txt"),
        format!("needle{}\n", "z".repeat(5000)),
    )
    .unwrap();
    let long = grep(&ws, json!({"pattern": "needle", "path": "long.txt"}));
    assert!(
        body(&long).len() < 400 && long.contains("…[truncated]"),
        "{}",
        long.len()
    );
}

#[test]
fn walks_honor_gitignore_and_skip_dependency_dirs() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join(".gitignore"), "/vendored/\n*.log\n").unwrap();
    for path in [
        "src/a.rs",
        "vendored/lib.rs",
        "src/debug.log",
        "web/node_modules/x.js",
        ".venv/lib/y.py",
        "src/.hidden.rs",
    ] {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, "needle\n").unwrap();
    }
    let listed = glob_tool(&ws, &json!({"pattern": "**/*"})).unwrap();
    let names: Vec<&str> = listed.lines().collect();
    assert!(
        names.contains(&"src/a.rs") && names.contains(&"src/.hidden.rs"),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("vendored")
            || n.ends_with(".log")
            || n.contains("node_modules")
            || n.contains(".venv")),
        "{names:?}"
    );
    let found = grep_tool(&ws, &json!({"pattern": "needle"})).unwrap();
    assert!(
        found.contains("src/a.rs")
            && !found.contains("vendored")
            && !found.contains("node_modules")
            && !found.contains(".venv"),
        "{found}"
    );
    let explicit = grep_tool(&ws, &json!({"pattern": "needle", "path": "vendored"})).unwrap();
    assert!(
        explicit.contains("vendored/lib.rs"),
        "an explicit path inside an ignored directory is still searched: {explicit}"
    );
}

#[test]
fn a_match_far_into_a_long_line_is_shown_not_cut_off() {
    let (dir, ws) = workspace();
    let line = format!("{}NEEDLE_HERE{}\n", "x".repeat(1000), "y".repeat(1000));
    fs::write(dir.path().join("min.js"), line).unwrap();
    let out = grep_tool(&ws, &json!({"pattern": "NEEDLE_HERE"})).unwrap();
    assert!(
        out.contains("NEEDLE_HERE") && out.contains("bytes skipped"),
        "{out}"
    );
    assert!(body(&out).len() < 600, "{}", body(&out).len());
    // A match near the start keeps the plain prefix.
    fs::write(
        dir.path().join("short.js"),
        format!("NEEDLE_HERE{}\n", "z".repeat(1000)),
    )
    .unwrap();
    let near = grep_tool(&ws, &json!({"pattern": "NEEDLE_HERE", "path": "short.js"})).unwrap();
    assert!(
        near.contains("short.js:1:NEEDLE_HERE") && !body(&near).contains("skipped"),
        "{near}"
    );
}

#[test]
fn grep_footer_names_the_files_a_capped_page_left_out() {
    let (dir, ws) = workspace();
    // A file past its per-file cap does not end the page: later files still appear on it.
    fs::write(dir.path().join("a_many.txt"), "hit\n".repeat(30)).unwrap();
    for i in 0..25 {
        fs::write(dir.path().join(format!("f{i:02}.txt")), "hit\n").unwrap();
    }
    let capped = grep(&ws, json!({"pattern": "hit"}));
    assert!(capped.contains("f24.txt:1:hit"), "{capped}");
    assert!(
        capped.contains(&format!(
            "per_file_cap={GREP_PER_FILE} reached in: a_many.txt"
        )),
        "{capped}"
    );
    assert!(!capped.contains("page_capped_by"), "{capped}");
    let whole = grep(
        &ws,
        json!({"pattern": "hit", "path": "a_many.txt", "max_results": 5}),
    );
    assert!(whole.contains("page_capped_by=match limit of 5"), "{whole}");
    let small = grep(&ws, json!({"pattern": "NOPE_NEVER"}));
    assert!(!small.contains("page_capped_by"), "{small}");
}

#[test]
fn capped_glob_keeps_deterministic_depth_first_paths() {
    let (dir, ws) = workspace();
    fs::create_dir(dir.path().join("a")).unwrap();
    let mut names = vec!["a.rs".to_string()];
    fs::write(dir.path().join("a.rs"), "").unwrap();
    for index in 0..MAX_LIST + 3 {
        let name = format!("a/{index:04}.rs");
        fs::write(dir.path().join(&name), "").unwrap();
        names.push(name);
    }
    names.sort_by(|a, b| Path::new(a).cmp(Path::new(b)));
    let expected = format!("{}\n", names[..MAX_LIST].join("\n"));
    for _ in 0..3 {
        assert_eq!(
            body(&glob_tool(&ws, &json!({"pattern": "**/*.rs"})).unwrap()),
            expected
        );
    }
}

#[cfg(unix)]
#[test]
fn anchored_globs_preserve_ignore_and_symlink_boundaries() {
    let (dir, ws) = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("mod.rs"), "needle\n").unwrap();
    symlink(outside.path(), dir.path().join("link")).unwrap();
    fs::write(dir.path().join(".gitignore"), "/ignored/\n").unwrap();
    for path in [
        "src/deep/mod.rs",
        "docs/mod.rs",
        "ignored/mod.rs",
        "target/mod.rs",
    ] {
        let file = dir.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "needle\n").unwrap();
    }
    for prefix in ["ignored", "target", "link"] {
        assert_eq!(
            body(&glob_tool(&ws, &json!({"pattern": format!("{prefix}/**/*.rs")})).unwrap()),
            "no matches\n"
        );
    }
    assert_eq!(
        body(&glob_tool(&ws, &json!({"pattern": "src/**/mod.rs"})).unwrap()),
        "src/deep/mod.rs\n"
    );
    assert_eq!(
        body(&glob_tool(&ws, &json!({"pattern": "{src,docs}/**/mod.rs"})).unwrap()),
        "docs/mod.rs\nsrc/deep/mod.rs\n"
    );
    assert_eq!(
        body(&grep(
            &ws,
            json!({"pattern": "needle", "glob": "src/**/mod.rs"})
        )),
        "src/deep/mod.rs:1:needle\n"
    );
}

#[test]
fn grep_binary_probe_boundary_and_reused_buffer() {
    let (dir, ws) = workspace();
    let mut binary = vec![b'x'; 1024 * 1024];
    binary[8191] = 0;
    fs::write(dir.path().join("a.bin"), binary).unwrap();
    let mut text = vec![b'x'; 8192];
    text.extend_from_slice(b"\0\nneedle\n");
    fs::write(dir.path().join("b.txt"), text).unwrap();
    fs::write(dir.path().join("c.txt"), "needle\n").unwrap();
    let result = grep(&ws, json!({"pattern": "needle"}));
    assert_eq!(
        body(&result).replace(GREP_WITNESS_WARNING, ""),
        "b.txt:2:needle\nc.txt:1:needle\n"
    );
    // b.txt carries a NUL past the 8 KiB probe, so it is scanned as text but cannot be
    // witnessed by the edit transaction (which rejects binary); the warning must say so.
    assert!(result.contains(GREP_WITNESS_WARNING), "{result}");
    assert!(result.contains("binary_files=1"));
}

#[test]
fn grep_cap_preserves_deterministic_depth_first_order() {
    let (dir, ws) = workspace();
    fs::create_dir(dir.path().join("a")).unwrap();
    fs::write(dir.path().join("a/z.rs"), "hit first\nhit second\n").unwrap();
    fs::write(dir.path().join("a.rs"), "hit later\n").unwrap();
    let expected = "a/z.rs:1:hit first\n";
    for _ in 0..3 {
        assert_eq!(
            body(&grep(&ws, json!({"pattern": "hit", "max_results": 1}))),
            expected
        );
    }
}

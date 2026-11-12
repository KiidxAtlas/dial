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
fn directory_read_identity_tracks_only_the_visible_listing() {
    use sha2::{Digest, Sha256};

    let (dir, ws) = workspace();
    fs::create_dir_all(dir.path().join("src/nested/deeper/bottom")).unwrap();
    fs::create_dir_all(dir.path().join("src/target")).unwrap();
    let identity = ws
        .read_identity("src")
        .expect("directory observation identity");
    let listing = run(&ws, "read", json!({"path":"src"}));
    assert!(!listing.is_error);
    assert_eq!(
        identity,
        <[u8; 32]>::from(Sha256::digest(listing.text.as_bytes()))
    );
    assert!(ws.file_identity(&dir.path().join("src")).is_none());
    for invisible in [
        "unrelated.txt",
        "src/target/generated.rs",
        "src/nested/deeper/bottom/hidden.rs",
    ] {
        fs::write(dir.path().join(invisible), "not listed").unwrap();
        assert_eq!(ws.read_identity("src"), Some(identity), "{invisible}");
    }
    let visible = dir.path().join("src/nested/visible.rs");
    fs::write(&visible, "source").unwrap();
    assert_ne!(ws.read_identity("src"), Some(identity));
    fs::remove_file(visible).unwrap();
    assert_eq!(ws.read_identity("src"), Some(identity));
    assert!(ws.read_identity("missing").is_none());
    let outside = tempfile::tempdir().unwrap();
    assert!(ws.read_identity(outside.path().to_str().unwrap()).is_none());
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

#[test]
fn multi_path_reads_report_each_target_and_fail_only_when_nothing_was_read() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mixed = read_paths_output(
        &ws,
        &json!({"paths": ["a.txt", "missing.txt"]}),
        &cancel,
        4096,
    )
    .unwrap();
    assert!(!mixed.is_error, "partial evidence is kept: {}", mixed.text);
    assert!(mixed.text.contains("alpha"));
    assert!(
        mixed.text.ends_with("[paths: 1 read, 1 failed, 0 omitted]"),
        "{}",
        mixed.text
    );
    let data = mixed.data.unwrap();
    assert_eq!(data["targets"][0], json!({"path":"a.txt","status":"read"}));
    assert_eq!(
        data["targets"][1],
        json!({"path":"missing.txt","status":"failed"})
    );
    assert_eq!(data["complete"], false);

    let failed = read_paths_output(
        &ws,
        &json!({"paths": ["gone.txt", "missing.txt"]}),
        &cancel,
        4096,
    )
    .unwrap();
    assert!(failed.is_error, "{}", failed.text);
    assert_eq!(failed.data.unwrap()["failed"], 2);

    let clean = read_paths_output(&ws, &json!({"paths": ["a.txt"]}), &cancel, 4096).unwrap();
    assert!(!clean.is_error && !clean.text.contains("[paths:"));
    assert_eq!(clean.data.unwrap()["complete"], true);

    // A budget too small for any row omits every target rather than reporting success.
    let omitted = read_paths_output(&ws, &json!({"paths": ["a.txt"]}), &cancel, 1).unwrap();
    assert!(omitted.is_error);
    assert_eq!(omitted.data.unwrap()["targets"][0]["status"], "omitted");
}

#[test]
fn multi_path_budget_is_shared_so_a_large_first_file_does_not_starve_later_targets() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "big.txt", 2000);
    fs::write(dir.path().join("small.txt"), "alpha\nbeta\n").unwrap();
    let mid: String = (1..=20).map(|n| format!("mid {n}\n")).collect();
    fs::write(dir.path().join("mid.txt"), mid).unwrap();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let budget = 16 * 1024;
    let out = read_paths_output(
        &ws,
        &json!({"paths": ["big.txt", "small.txt", "mid.txt"]}),
        &cancel,
        budget,
    )
    .unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.data.as_ref().unwrap()["read"], 3, "{}", out.text);
    assert!(out.text.contains("     1\tline 1\n"), "{}", out.text);
    assert!(
        out.text.contains("alpha") && out.text.contains("mid 20"),
        "{}",
        out.text
    );
    assert!(out.text.len() <= budget, "{}", out.text.len());

    // The clipped first file names the exact selector that continues it from its first unshown row.
    let next = out
        .text
        .split("continue with `big.txt:")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .expect("big.txt continuation");
    let from: usize = next.trim_end_matches('-').parse().unwrap();
    assert!(from > 1 && from < 2000, "{next}");
    // Only the rows the output showed witness edits: the first row yes, the continuation row no.
    let big = dir.path().join("big.txt");
    let big_text = fs::read_to_string(&big).unwrap();
    assert!(crate::tools::edit::editing::seen_guard(&ws, &big, &big_text, &[(1, 1)]).is_ok());
    assert!(
        crate::tools::edit::editing::seen_guard(&ws, &big, &big_text, &[(from, from)]).is_err()
    );

    // The continuation selector then reads on from exactly that row.
    let rest = run(&ws, "read", json!({"path": format!("big.txt:{next}")}));
    assert!(
        rest.text.contains(&format!("{from:>6}\tline {from}\n")),
        "{}",
        rest.text
    );
    let mid_path = dir.path().join("mid.txt");
    let mid_text = fs::read_to_string(&mid_path).unwrap();
    assert!(
        crate::tools::edit::editing::seen_guard(&ws, &mid_path, &mid_text, &[(20, 20)]).is_ok()
    );
}

#[test]
fn multi_path_total_stays_bounded_and_clipped_or_omitted_targets_name_their_continuation() {
    let (dir, ws) = workspace();
    let names: Vec<String> = (0..16).map(|n| format!("t{n:02}.txt")).collect();
    for name in &names {
        numbered_file(&dir, name, 400);
    }
    let cancel = std::sync::atomic::AtomicBool::new(false);

    // Sixteen large files share the default output budget: each is clipped, none starves.
    let budget = MAX_READ_OUTPUT;
    let out = read_paths_output(&ws, &json!({"paths": names}), &cancel, budget).unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(out.data.as_ref().unwrap()["read"], 16, "{}", out.text);
    assert!(out.text.len() <= budget, "{}", out.text.len());
    for name in &names {
        assert!(
            out.text.contains(&format!("continue with `{name}:")),
            "{name}: {}",
            out.text
        );
    }

    // A budget that cannot show every target reads the first, omits the rest by name, and
    // leaves the omitted rows without edit witnesses. A fresh workspace keeps the earlier
    // full-budget reads from witnessing these rows.
    let tight_budget = 2048;
    let fresh = Workspace::new(dir.path())
        .unwrap()
        .with_edit_mode(crate::tools::EditMode::Replace);
    let tight = read_paths_output(&fresh, &json!({"paths": names}), &cancel, tight_budget).unwrap();
    let data = tight.data.as_ref().unwrap();
    assert_eq!(
        (data["read"].clone(), data["omitted"].clone()),
        (json!(1), json!(15)),
        "{}",
        tight.text
    );
    assert!(
        tight.text.contains(&format!("paths={}", json!(names[1..]))),
        "{}",
        tight.text
    );
    assert!(tight.text.len() <= tight_budget, "{}", tight.text.len());
    let shown = dir.path().join(&names[0]);
    let shown_text = fs::read_to_string(&shown).unwrap();
    assert!(
        crate::tools::edit::editing::seen_guard(&fresh, &shown, &shown_text, &[(1, 1)]).is_ok()
    );
    assert!(
        crate::tools::edit::editing::seen_guard(&fresh, &shown, &shown_text, &[(400, 400)])
            .is_err()
    );
    let omitted = dir.path().join(&names[1]);
    let omitted_text = fs::read_to_string(&omitted).unwrap();
    assert!(
        crate::tools::edit::editing::seen_guard(&fresh, &omitted, &omitted_text, &[(1, 1)])
            .is_err()
    );
    // The smallest budget a read accepts shows no row, and a failing target stays one bounded line.
    let mut failing = names[..15].to_vec();
    failing.push("gone.txt".into());
    let minimal = read_paths_output(&fresh, &json!({"paths": failing}), &cancel, 1024).unwrap();
    assert!(minimal.is_error, "{}", minimal.text);
    assert_eq!(
        minimal.data.as_ref().unwrap()["omitted"],
        16,
        "{}",
        minimal.text
    );
    assert!(minimal.text.len() <= 1024, "{}", minimal.text.len());
    let error = read_paths_output(
        &fresh,
        &json!({"paths": ["gone.txt", &"x".repeat(900)]}),
        &cancel,
        48 * 1024,
    )
    .unwrap();
    assert!(
        error
            .text
            .lines()
            .all(|line| line.len() <= ERROR_LINE_CAP + 3),
        "{}",
        error.text
    );
}

#[test]
fn multi_path_demand_follows_selectors_offset_and_limit_without_reading_rows() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "big.txt", 2000);
    let none = json!({});
    let whole = target_demand(&ws, &none, "big.txt");
    let ranged = target_demand(&ws, &none, "big.txt:1-3");
    assert_eq!(ranged, 3 * PATH_ROW_BOUND + PATH_ROOM_FLOOR);
    assert!(ranged < whole, "a 3-line range must not reserve the file");
    // An open-ended range and a whole file both reach `limit` lines (default 2000) and the end.
    assert_eq!(target_demand(&ws, &none, "big.txt:5-"), whole);
    assert_eq!(
        target_demand(&ws, &json!({"offset": 5, "limit": 2}), "big.txt"),
        2 * PATH_ROW_BOUND + PATH_ROOM_FLOOR
    );
    assert_eq!(
        target_demand(&ws, &json!({"limit": 2}), "big.txt:5-"),
        2 * PATH_ROW_BOUND + PATH_ROOM_FLOOR
    );
    // A closed selector is not stretched by `limit`: its 11 lines, capped at the file size.
    assert_eq!(
        target_demand(&ws, &json!({"limit": 2}), "big.txt:10-20"),
        (11 * PATH_ROW_BOUND).min(whole - PATH_ROOM_FLOOR) + PATH_ROOM_FLOOR
    );
    assert_eq!(target_demand(&ws, &none, "missing.txt"), PATH_ROOM_FLOOR);
}

#[test]
fn multi_path_handle_targets_plan_and_read_like_their_paths() {
    let (dir, ws) = workspace();
    numbered_file(&dir, "big.txt", 2000);
    fs::write(dir.path().join("small.txt"), "alpha\n").unwrap();
    let none = json!({});
    let handle = ws.file_handle("big.txt").unwrap();
    assert_eq!(
        target_demand(&ws, &none, &handle),
        target_demand(&ws, &none, "big.txt")
    );
    assert_eq!(
        target_demand(&ws, &none, &format!("{handle}:1-3")),
        target_demand(&ws, &none, "big.txt:1-3")
    );
    // The handle target shares the budget like its path: the small target after it is whole and
    // the handle's own continuation names the file by its path.
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let out = read_paths_output(
        &ws,
        &json!({"paths": [handle, "small.txt"]}),
        &cancel,
        16 * 1024,
    )
    .unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("alpha"), "{}", out.text);
    assert!(out.text.contains("continue with `big.txt:"), "{}", out.text);
    assert_eq!(out.data.as_ref().unwrap()["read"], 2, "{}", out.text);
}

#[test]
fn multi_path_notice_stays_within_budget_when_full_target_list_would_not() {
    let (_dir, ws) = workspace();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    // Sixteen long names, none of which exist: their full JSON list is far larger than 1 KiB.
    let names: Vec<String> = (0..16)
        .map(|n| format!("{n:02}-{}.txt", "x".repeat(300)))
        .collect();
    let out = read_paths_output(&ws, &json!({"paths": names}), &cancel, 1024).unwrap();
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.len() <= 1024, "{}", out.text.len());
    assert!(
        out.text.contains("[paths 1-16 of 16 omitted"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("paths=["), "{}", out.text);
    // The structured list still carries every target by name, in order.
    let data = out.data.as_ref().unwrap();
    assert_eq!(data["omitted"], 16);
    assert_eq!(data["targets"][15]["path"], names[15]);
    assert!(compact_notice(15, 16).len() <= COMPACT_NOTICE_BOUND);
}

#[test]
fn multi_path_cuts_only_non_witnessing_sections_that_overrun_their_share() {
    let (dir, ws) = workspace();
    let cancel = std::sync::atomic::AtomicBool::new(false);
    fs::write(dir.path().join("small.txt"), "alpha\n").unwrap();
    // A listing ignores the reader's room, so long names overrun a share on their own.
    fs::create_dir(dir.path().join("long")).unwrap();
    for n in 0..60 {
        let name = format!("{n:03}-{}.txt", "y".repeat(110));
        fs::write(dir.path().join("long").join(name), "").unwrap();
    }
    let outline: String = (0..800).map(|n| format!("fn decl_{n}() {{}}\n")).collect();
    fs::write(dir.path().join("outline.rs"), outline).unwrap();
    let json: String = (0..4000).map(|n| format!("{{\"n\":{n}}},")).collect();
    fs::write(dir.path().join("data.json"), format!("[{json}{{}}]")).unwrap();
    let blob: Vec<u8> = (0..40_000).map(|n| (n % 251) as u8).collect();
    fs::write(dir.path().join("blob.bin"), blob).unwrap();
    let budget = 4096;
    // Directories are the reader that overran; the others already stay within a share, and the
    // guard leaves their text whole. Every case keeps the total bound and the later target.
    for target in ["long", "outline.rs:outline", "data.json", "blob.bin"] {
        let out = read_paths_output(
            &ws,
            &json!({"paths": [target, "small.txt"]}),
            &cancel,
            budget,
        )
        .unwrap();
        assert!(out.text.len() <= budget, "{target}: {}", out.text.len());
        assert!(out.text.contains("alpha"), "{target}: {}", out.text);
        if target == "long" {
            assert!(
                out.text.contains("cut to its share of the budget"),
                "{}",
                out.text
            );
        }
    }
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
fn capped_lines_drop_a_crlf_terminator_and_never_split_a_character() {
    let read = |input: &[u8], cap: usize| {
        let mut reader = io::BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        let truncated = read_line_capped(&mut reader, &mut buf, cap).unwrap();
        (String::from_utf8(buf).unwrap(), truncated)
    };
    // A line exactly at the cap is whole, whether LF, CRLF or CRLF split across chunks.
    assert_eq!(read(b"abcd\n", 4), ("abcd".into(), Some(false)));
    assert_eq!(read(b"abcd\r\n", 4), ("abcd".into(), Some(false)));
    assert_eq!(read(b"abc\r\nx", 3), ("abc".into(), Some(false)));
    assert_eq!(read(b"a\r\r\n", 9), ("a\r".into(), Some(false)));
    assert_eq!(read(b"abcde\r\n", 4), ("abcd".into(), Some(true)));
    // A cut inside a multibyte character backs off to the character boundary.
    assert_eq!(read("aé€\n".as_bytes(), 4), ("aé".into(), Some(true)));
    assert_eq!(read("aé€".as_bytes(), 5), ("aé".into(), Some(true)));
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
    // The receipt carries size, line count and the on-disk hash, so no reread is needed.
    assert!(
        out.text.contains("created 2 supplied bytes (1 lines)")
            && out.text.contains("[Source raw SHA256:")
            && out.text.contains("rereading it adds nothing"),
        "{}",
        out.text
    );
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
    // The exact first-line location leads; any fuzzy closest match follows it.
    let first = err.find("at line 2").expect("first-line location");
    assert!(
        err.find("Closest match")
            .is_none_or(|closest| first < closest),
        "{err}"
    );
    let gone = edit_err(&ws, "a.rs", "fn missing_entirely() {}");
    assert!(gone.contains("Not even its first line"), "{gone}");
}

#[test]
fn identical_old_and_new_text_absent_from_the_file_is_named_before_any_search() {
    let (dir, ws) = workspace();
    let file = dir.path().join("a.py");
    fs::write(&file, "WIDTH = 10\n\ndef draw(self):\n    pass\n").unwrap();
    let code = "def draw(self):\n    paint(SIDEBAR_WIDTH)";
    let out = run(
        &ws,
        "edit",
        json!({"path": "a.py", "old_string": code, "new_string": code}),
    );
    assert!(out.is_error, "{}", out.text);
    assert!(
        out.text
            .starts_with("old_string and new_string are identical")
            && out.text.contains("earlier edit")
            && out.text.contains("at line 3")
            && !out.text.contains("Closest match"),
        "{}",
        out.text
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "WIDTH = 10\n\ndef draw(self):\n    pass\n"
    );
}

#[test]
fn an_edit_whose_old_text_covers_most_of_the_file_points_to_write() {
    let (dir, ws) = workspace();
    let body: String = (0..50).map(|n| format!("value_{n} = {n}\n")).collect();
    fs::write(dir.path().join("a.py"), &body).unwrap();
    assert!(!run(&ws, "read", json!({"path": "a.py"})).is_error);
    // A whole-file old_string that matches nothing misses and names `write`.
    let stale: String = (0..50)
        .map(|n| format!("other_{n} = {}\n", n * 7 + 3))
        .collect();
    let err = edit_err(&ws, "a.py", stale.trim_end());
    assert!(
        err.contains("covers most of the file") && err.contains("`write`"),
        "{err}"
    );
    // A small miss in the same file keeps the ordinary hint.
    let small = edit_err(&ws, "a.py", "value_7 = 77");
    assert!(!small.contains("covers most of the file"), "{small}");
    // A successful whole-file edit still lands and carries the hint for next time.
    let out = run(
        &ws,
        "edit",
        json!({"path": "a.py", "old_string": body.trim_end(), "new_string": "done = 1"}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("covers most of the file"), "{}", out.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.py")).unwrap(),
        "done = 1\n"
    );
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

#[test]
fn rewrite_advice_flags_mostly_unchanged_overwrites_of_non_trivial_files() {
    let old: String = (0..60).map(|i| format!("line {i}\n")).collect();
    let mostly_same = old.replace("line 7\n", "changed\n");
    assert!(
        rewrite_advice(&old, &mostly_same)
            .unwrap()
            .contains("59 of 60 lines were unchanged")
    );
    let replaced: String = (0..60).map(|i| format!("new {i}\n")).collect();
    assert_eq!(rewrite_advice(&old, &replaced), None);
    // Small files are cheap to regenerate; no advice.
    assert_eq!(rewrite_advice("a\nb\n", "a\nc\n"), None);
}

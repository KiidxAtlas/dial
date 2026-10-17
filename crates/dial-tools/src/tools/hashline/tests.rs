use super::*;

fn lines(text: &str) -> Vec<String> {
    split_lines(text).into_iter().map(str::to_owned).collect()
}

#[test]
fn tag_is_four_uppercase_hex_and_ignores_crlf() {
    let tag = tag_of("a\nb\n");
    assert_eq!(tag.len(), 4);
    assert!(
        tag.chars()
            .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c))
    );
    assert_eq!(tag, tag_of(&normalize("a\r\nb\r\n")));
    assert_ne!(tag, tag_of("a\nc\n"));
    assert_eq!(split_lines(""), Vec::<&str>::new());
    assert_eq!(split_lines("a\n\n"), vec!["a", ""]);
    assert_eq!(split_lines("a"), vec!["a"]);
}

#[test]
fn parser_reads_every_operation_form() {
    let patch = "*** Begin Patch\n[a.rs#1a2B]\nPUT 1.=2:\n+x\n+\n++plus\n+-minus\nPUT <1:\n+h\nPUT >3:\n+t\nPUT >$:\n+e\nPUT 4*:\n+b\nPUT >5*:\n+ab\nCUT 6.=7\nCUT 8* @n\nPUT <2\nPUT >9 @n\nPUT >$ @n\nPUT 1.=1 @n\nPUT 2* @n\n[b.rs#FFFF]\nREM\n[c.rs#0000]\nMV \"new dir/c.rs\"\n*** End Patch\n";
    let sections = parse_patch(patch).unwrap();
    assert_eq!(sections.len(), 3);
    assert_eq!(sections[0].tag, "1A2B");
    assert_eq!(sections[0].ops.len(), 13);
    match &sections[0].ops[0].kind {
        OpKind::Put {
            anchor: Anchor::Range(1, 2),
            src: Src::Body(body),
        } => {
            assert_eq!(body, &["x", "", "+plus", "-minus"]);
        }
        other => panic!("{other:?}"),
    }
    assert!(sections[1].rem);
    assert_eq!(sections[2].mv.as_deref(), Some("new dir/c.rs"));
}

#[test]
fn anchor_aliases_preserve_read_before_edit_checks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, "a\nb\nc\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("a.txt").unwrap();
    let tag = ws.store().record(&path, "a\nb\nc\n", &[(1, 1)]);
    for target in ["2", "2,3"] {
        let patch = format!("[a.txt#{tag}]\nPUT {target}:\n+x\n");
        assert!(
            edit(&ws, &patch)
                .unwrap_err()
                .contains("have not been shown")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
    }
    ws.store().record_all(&path, "a\nb\nc\n");
    edit(&ws, &format!("[a.txt#{tag}]\nPUT 2,3:\n+x\n")).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "a\nx\n");
}

#[test]
fn unseen_anchor_error_shows_the_lines_so_the_retry_needs_no_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("a.txt").unwrap();
    let tag = ws
        .store()
        .record(&path, "one\ntwo\nthree\nfour\n", &[(1, 1)]);
    let patch = format!("[a.txt#{tag}]\nPUT 2.=3:\n+TWO\n+THREE\n");
    let error = edit(&ws, &patch).unwrap_err();
    assert!(error.contains("have not been shown"), "{error}");
    assert!(error.contains("2:two\n3:three\n"), "{error}");
    assert!(!error.contains("4:four"), "{error}");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "one\ntwo\nthree\nfour\n"
    );
    edit(&ws, &patch).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "one\nTWO\nTHREE\nfour\n"
    );
}

#[test]
fn stale_remove_names_the_fresh_tag_and_the_range_to_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, "a\nb\nc\n").unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("a.txt").unwrap();
    let tag = ws.store().record(&path, "a\nb\nc\n", &[(1, 3)]);
    fs::write(&path, "a\nB\nc\n").unwrap();
    let error = edit(&ws, &format!("[a.txt#{tag}]\nREM\n")).unwrap_err();
    let fresh = tag_of("a\nB\nc\n");
    assert!(error.contains(&format!("[a.txt#{fresh}]")), "{error}");
    assert!(error.contains("`a.txt:1-3`"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "a\nB\nc\n");
}

#[test]
fn parser_accepts_unambiguous_line_and_comma_range_aliases() {
    for (alias, canonical) in [("2", "2.=2"), ("2,3", "2.=3"), ("2, 3", "2.=3")] {
        assert_eq!(
            parse_anchor(alias).unwrap(),
            parse_anchor(canonical).unwrap()
        );
        for verb in ["PUT", "CUT"] {
            let suffix = if verb == "PUT" { ":\n+x\n" } else { "\n" };
            let alias_patch = format!("[a.rs#1234]\n{verb} {alias}{suffix}");
            let canonical_patch = format!("[a.rs#1234]\n{verb} {canonical}{suffix}");
            assert_eq!(
                parse_patch(&alias_patch).unwrap()[0].ops[0].kind,
                parse_patch(&canonical_patch).unwrap()[0].ops[0].kind
            );
        }
    }
    for invalid in ["0", "0,2", "3,2", "2,3,4", "2,3=2", "2.=3=2"] {
        assert!(parse_anchor(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn parser_rejects_malformed_patches_with_actionable_messages() {
    let cases: &[(&str, &str)] = &[
        ("", "empty patch"),
        ("PUT 1.=1:\n+x\n", "section header"),
        ("[a.rs]\nPUT 1.=1:\n+x\n", "no #TAG"),
        ("[a.rs#12]\nPUT 1.=1:\n+x\n", "four hex"),
        (
            "[a.rs#1234]\nPUT 1.=1:\nPUT 2.=2:\n+x\n",
            "at least one `+` row",
        ),
        ("[a.rs#1234]\nPUT 3.=1:\n+x\n", "reversed"),
        ("[a.rs#1234]\nPUT 0.=1:\n+x\n", "1-based"),
        ("[a.rs#1234]\nPUT 1.=2 \n", "needs `@name`"),
        ("[a.rs#1234]\nPUT 2*\n", "needs `@name`"),
        ("[a.rs#1234]\nPUT 2,3=2:\n+x\n", "range end"),
        ("[a.rs#1234]\nCUT >2\n", "CUT takes"),
        ("[a.rs#1234]\nPUT >2 @bad name\n", "invalid register name"),
        ("[a.rs#1234]\n@@ -1 +1 @@\n", "unified diff"),
        ("[a.rs#1234]\n-old\n", "unified diff"),
        ("[a.rs#1234]\nCUT 1.=1\n+b\n", "outside"),
        ("[a.rs#1234]\n", "no operations"),
        ("[a.rs#1234]\nFROB 3\n", "unrecognized"),
    ];
    for (patch, expected) in cases {
        let err = parse_patch(patch).unwrap_err();
        assert!(err.contains(expected), "{patch:?} -> {err}");
    }
    assert!(
        parse_patch(&"x".repeat(MAX_INPUT + 1))
            .unwrap_err()
            .contains("limit")
    );
}

#[test]
fn missing_range_end_teaches_a_valid_retry() {
    let error = parse_patch("[a.rs#1234]\nPUT 2.=:\n+fixed\n").unwrap_err();
    assert!(error.contains("line 2: range end: expected a line number"));
    assert!(error.contains("PUT 2.=2:"), "{error}");
    assert!(error.contains("PUT >2:"), "{error}");
    assert!(parse_patch("[a.rs#1234]\nPUT 2.=2:\n+fixed\n").is_ok());
    assert!(parse_patch("[a.rs#1234]\nPUT >2:\n+fixed\n").is_ok());
}

#[test]
fn block_end_on_brace_languages() {
    let src = lines(
        "use x;\n\n#[derive(Debug)]\nstruct A {\n    f: i32,\n}\n\nfn run(\n    a: i32,\n) -> String {\n    let s = \"}\"; // }\n    let c = '{';\n    if a > 0 {\n        s.len();\n    }\n    format!(\"x\")\n}\n\nfn tiny() {}\nfn alloc()\n{\n    body();\n}\n",
    );
    assert_eq!(
        block_end(&src, 3, "rs").unwrap(),
        6,
        "attribute belongs to the struct block"
    );
    assert_eq!(block_end(&src, 4, "rs").unwrap(), 6);
    assert_eq!(
        block_end(&src, 8, "rs").unwrap(),
        17,
        "multi-line signature, braces in strings/comments ignored"
    );
    assert_eq!(block_end(&src, 13, "rs").unwrap(), 15);
    assert!(
        block_end(&src, 19, "rs")
            .unwrap_err()
            .contains("single-line")
    );
    assert_eq!(
        block_end(&src, 20, "rs").unwrap(),
        23,
        "Allman brace on the next line"
    );
    assert!(
        block_end(&src, 6, "rs")
            .unwrap_err()
            .contains("closing delimiter")
    );
    assert!(block_end(&src, 2, "rs").unwrap_err().contains("blank"));
    assert!(
        block_end(&src, 1, "rs")
            .unwrap_err()
            .contains("single-line")
    );
    assert!(block_end(&src, 99, "rs").unwrap_err().contains("outside"));
    let open = lines("fn f() {\n    x();\n");
    assert!(
        block_end(&open, 1, "rs")
            .unwrap_err()
            .contains("never closed")
    );
    let js = lines("const a = [\n  1,\n  2,\n];\nconst s = 'it\\'s (odd';\n");
    assert_eq!(block_end(&js, 1, "js").unwrap(), 4);
    assert!(block_end(&js, 5, "js").unwrap_err().contains("single-line"));
}

#[test]
fn block_end_on_indentation_languages() {
    let src = lines(
        "@cache\ndef greet(name):\n    print(f\"hi ({name}\")\n\n    return 1\n\ngreet(\"world\")\ndef long(a,\n         b):\n    # ) comment\n    pass\nx = 1\nclass K:\n    def m(self):\n        pass\n\n    def n(self):\n        pass\n",
    );
    assert_eq!(
        block_end(&src, 1, "py").unwrap(),
        5,
        "decorator + body, trailing blank excluded"
    );
    assert_eq!(block_end(&src, 2, "py").unwrap(), 5);
    assert!(
        block_end(&src, 7, "py")
            .unwrap_err()
            .contains("single-line")
    );
    assert_eq!(
        block_end(&src, 8, "py").unwrap(),
        11,
        "paren signature ending in a colon continues into the body"
    );
    assert_eq!(block_end(&src, 13, "py").unwrap(), 18);
    assert_eq!(block_end(&src, 14, "py").unwrap(), 15);
}

#[test]
fn block_end_on_markdown_headings() {
    let md = lines(
        "# Title\nintro\n\n## A\ntext\n\n### A1\ndeep\n\n```\n# not a heading\n```\n\n## B\nb\n\n#nospace\n",
    );
    assert_eq!(
        block_end(&md, 1, "md").unwrap(),
        17,
        "level-1 heading spans everything"
    );
    assert_eq!(
        block_end(&md, 4, "md").unwrap(),
        12,
        "stops before ## B; fence content ignored; trailing blank excluded"
    );
    assert_eq!(block_end(&md, 7, "md").unwrap(), 12);
    assert_eq!(block_end(&md, 14, "md").unwrap(), 17);
    let bare = lines("# Only\n");
    assert!(block_end(&bare, 1, "md").unwrap_err().contains("no body"));
}

#[test]
fn line_map_matches_unchanged_lines_around_edits() {
    let base = lines("a\nb\nc\nd\ne\nf\n");
    let cur = lines("a\nX\nY\nc\nd\nf\ng\n");
    let map = line_map(&base, &cur).unwrap();
    assert_eq!(map, vec![Some(0), None, Some(3), Some(4), None, Some(5)]);
    assert_eq!(
        hunks(&map, cur.len()),
        vec![(1, 2, 1, 3), (4, 5, 5, 5), (6, 6, 6, 7)]
    );
    let same = line_map(&base, &base).unwrap();
    assert!(same.iter().enumerate().all(|(i, m)| *m == Some(i)));
    assert_eq!(line_map(&[], &cur).unwrap().len(), 0);
    let huge: Vec<String> = (0..MAX_DIFF_LINES + 1).map(|i| i.to_string()).collect();
    let mut changed = huge.clone();
    changed[MAX_DIFF_LINES / 2] = "external edit".into();
    let mapped = line_map(&huge, &changed).expect("small middle diff in large file");
    assert_eq!(mapped[MAX_DIFF_LINES / 2], None);
}

#[test]
fn myers_finds_a_minimal_script_on_larger_inputs() {
    let a: Vec<u32> = (0..400).collect();
    let mut b = a.clone();
    b.drain(100..110);
    b.insert(200, 9999);
    b.swap(300, 301);
    let pairs = myers(&a, &b, MAX_DIFF_D).unwrap();
    assert!(pairs.iter().all(|&(i, j)| a[i] == b[j]));
    assert!(pairs.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
    assert!(pairs.len() >= 400 - 10 - 1);
    assert!(
        myers(
            &(0..50).collect::<Vec<_>>(),
            &(100..150).collect::<Vec<_>>(),
            10
        )
        .is_none()
    );
}

#[test]
fn translate_requires_unchanged_contiguous_anchors() {
    let base = lines("a\nb\nc\nd\ne\n");
    let cur = lines("new\na\nb\nc\nd\ne\n");
    let map = line_map(&base, &cur).unwrap();
    let mut ok = vec![
        Edit {
            lo: 2,
            hi: 3,
            insert: false,
            lines: vec![],
        },
        Edit {
            lo: 5,
            hi: 5,
            insert: true,
            lines: vec![],
        },
        Edit {
            lo: 1,
            hi: 1,
            insert: true,
            lines: vec![],
        },
    ];
    translate(&mut ok, &map, cur.len()).unwrap();
    assert_eq!((ok[0].lo, ok[0].hi), (3, 4));
    assert_eq!(ok[1].lo, 6);
    assert_eq!(ok[2].lo, 2);
    // Inserting at the head would land among changed lines: refused.
    let mut head = vec![Edit {
        lo: 0,
        hi: 0,
        insert: true,
        lines: vec![],
    }];
    assert!(translate(&mut head, &map, cur.len()).is_err());
    // A range that straddles changed lines is refused.
    let changed = lines("a\nB\nc\nd\ne\n");
    let map = line_map(&base, &changed).unwrap();
    let mut straddle = vec![Edit {
        lo: 1,
        hi: 3,
        insert: false,
        lines: vec![],
    }];
    assert_eq!(
        translate(&mut straddle, &map, changed.len()).unwrap_err(),
        vec![(1, 3)]
    );
}

#[test]
fn overlap_detection() {
    let r = |lo, hi| Edit {
        lo,
        hi,
        insert: false,
        lines: vec![],
    };
    let i = |g| Edit {
        lo: g,
        hi: g,
        insert: true,
        lines: vec![],
    };
    assert!(check_overlap(&[r(1, 3), r(4, 5), i(3), i(5), i(0)]).is_ok());
    assert!(
        check_overlap(&[r(1, 3), r(3, 5)])
            .unwrap_err()
            .contains("overlapping")
    );
    assert!(check_overlap(&[r(2, 2), r(2, 2)]).is_err());
    assert!(
        check_overlap(&[r(2, 5), i(3)])
            .unwrap_err()
            .contains("inside")
    );
    assert!(check_overlap(&[r(2, 5), i(1), i(5)]).is_ok());
}

#[test]
fn apply_orders_inserts_around_replacements_and_tracks_seen() {
    let cur = lines("a\nb\nc\nd\n");
    let seen = vec![true, false, true, false];
    let edits = vec![
        Edit {
            lo: 2,
            hi: 3,
            insert: false,
            lines: vec!["X".into()],
        },
        Edit {
            lo: 1,
            hi: 1,
            insert: true,
            lines: vec!["after-a".into()],
        },
        Edit {
            lo: 3,
            hi: 3,
            insert: true,
            lines: vec!["after-c".into()],
        },
        Edit {
            lo: 0,
            hi: 0,
            insert: true,
            lines: vec!["top1".into()],
        },
        Edit {
            lo: 0,
            hi: 0,
            insert: true,
            lines: vec!["top2".into()],
        },
        Edit {
            lo: 4,
            hi: 4,
            insert: true,
            lines: vec!["end".into()],
        },
    ];
    let out = apply_edits(&cur, &seen, &edits);
    assert_eq!(
        out.lines,
        ["top1", "top2", "a", "after-a", "X", "after-c", "d", "end"]
    );
    assert_eq!(out.seen, [true, true, true, true, true, true, false, true]);
    assert_eq!((out.added, out.removed), (6, 2));
    let cut = apply_edits(
        &cur,
        &seen,
        &[Edit {
            lo: 2,
            hi: 3,
            insert: false,
            lines: vec![],
        }],
    );
    assert_eq!(cut.lines, ["a", "d"]);
    assert_eq!(cut.regions, vec![(2, 1)]);
}

#[test]
fn seen_ranges_merge_and_report_the_first_gap() {
    let mut seen = Seen::from_ranges(&[(5, 8), (1, 3), (4, 4), (20, 22)]);
    assert_eq!(seen.0, vec![(1, 8), (20, 22)]);
    assert_eq!(seen.first_unseen(2, 7), None);
    assert_eq!(seen.first_unseen(6, 12), Some((9, 12)));
    assert_eq!(seen.first_unseen(9, 25), Some((9, 19)));
    assert_eq!(seen.first_unseen(21, 25), Some((23, 25)));
    seen.add(9, 19);
    assert_eq!(seen.first_unseen(1, 22), None);
    assert_eq!(
        ranges_from_flags(&[true, true, false, true]),
        vec![(1, 2), (4, 4)]
    );
}

#[test]
fn store_bounds_snapshots_and_unions_seen() {
    let mut store = EditStore::default();
    let path = Path::new("/w/a.txt");
    let tag = store.record(path, "a\nb\nc\n", &[(1, 1)]);
    assert_eq!(store.record(path, "a\nb\nc\n", &[(3, 3)]), tag);
    assert_eq!(
        store.snapshot(path, &tag).unwrap().seen.0,
        vec![(1, 1), (3, 3)]
    );
    for i in 0..SNAPSHOTS_PER_FILE + 3 {
        store.record(path, &format!("v{i}\n"), &[]);
    }
    assert_eq!(store.files[path].len(), SNAPSHOTS_PER_FILE);
    assert!(
        store.snapshot(path, &tag).is_none(),
        "oldest snapshot evicted"
    );
    for i in 0..MAX_FILES + 5 {
        store.record(Path::new(&format!("/w/f{i}")), "x\n", &[]);
    }
    assert_eq!(store.files.len(), MAX_FILES);
    store.forget(Path::new("/w/f70"));
    assert!(!store.files.contains_key(Path::new("/w/f70")));
}

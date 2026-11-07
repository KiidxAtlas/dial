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
fn successful_edit_model_projection_keeps_fresh_anchors_and_raw_diff() {
    let dir = tempfile::tempdir().unwrap();
    // Key the read record by the workspace's canonical root (macOS tempdirs sit behind /var).
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.root().join("a.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let tag = ws.store().record_all(&path, "one\ntwo\nthree\n");
    let output = edit_output_with(
        &ws,
        &format!("[a.txt#{tag}]\nPUT 2.=2:\n+TWO\n"),
        true,
        &super::super::editing::Execution::default(),
    )
    .unwrap();
    let history = crate::tools::surface::outcome::message_for("edit", "call", &output);
    let model = history["content"].as_str().unwrap();
    assert!(output.text.contains("change preview"));
    assert!(output.text.contains("-two\n+TWO"));
    assert!(!model.contains("change preview"), "{model}");
    assert!(model.contains("2:TWO\n"), "{model}");
    let fresh = tag_of("one\nTWO\nthree\n");
    assert!(model.contains(&format!("[a.txt#{fresh}]")), "{model}");
    // The retained anchor is immediately usable without a redundant read.
    edit(&ws, &format!("[a.txt#{fresh}]\nPUT 2.=2:\n+again\n")).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "one\nagain\nthree\n");
}

#[test]
fn incomplete_numbered_previews_keep_only_distinct_diff_evidence() {
    for (replacement, diff_for_model) in [
        ("x".repeat(PREVIEW_LINE_CAP + 1), true),
        ("x\n".repeat(PREVIEW_MAX_ROWS + 1), false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let path = ws.root().join("a.txt");
        fs::write(&path, "old\n").unwrap();
        let tag = ws.store().record_all(&path, "old\n");
        let body = replacement
            .lines()
            .map(|line| format!("+{line}\n"))
            .collect::<String>();
        let output = edit_output_with(
            &ws,
            &format!("[a.txt#{tag}]\nPUT 1.=1:\n{body}"),
            true,
            &super::super::editing::Execution::default(),
        )
        .unwrap();
        let history = crate::tools::surface::outcome::message_for("edit", "call", &output);
        let model = history["content"].as_str().unwrap();
        assert!(output.text.contains("change preview"));
        // A cut line keeps the diff; a preview cut after its row limit names the omitted
        // count instead of repeating the change as a diff.
        assert_eq!(model.contains("change preview"), diff_for_model, "{model}");
        if !diff_for_model {
            assert!(model.contains("[preview truncated;"), "{model}");
        }
    }
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
    let capture = crate::runtime::execution::Capture::begin();
    let error = edit(&ws, &patch).unwrap_err();
    let retry = capture.finish(false).native_edit_retry.unwrap();
    assert!(native_edit_retry_ready(&ws, &retry));
    assert_eq!(retry.requirements[0].ranges, vec![(2, 3)]);
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
fn retry_witnesses_pin_notebook_raw_revision_even_when_source_matches() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("a.ipynb").unwrap();
    let mut notebook = serde_json::json!({
        "nbformat":4,"nbformat_minor":5,"metadata":{},
        "cells":[{"id":"cell-one","cell_type":"code","metadata":{},"source":["x = 1\n"],
            "execution_count":null,"outputs":[]}]
    });
    let raw = notebook.to_string();
    fs::write(&path, &raw).unwrap();
    let source = super::super::edit_source::Source::load(&path, Some(&raw)).unwrap();
    let capture = crate::runtime::execution::Capture::begin();
    assert!(source.check_ranges(&ws, &path, &[(1, 1)]).is_err());
    let retry = capture.finish(false).native_edit_retry.unwrap();
    assert!(!native_edit_retry_ready(&ws, &retry));
    let read = || crate::tools::execute(&ws, "read", &serde_json::json!({"path":"a.ipynb"}), 20);
    assert!(!read().is_error);
    assert!(native_edit_retry_ready(&ws, &retry));
    notebook["metadata"] = serde_json::json!({"changed":true});
    let raw = notebook.to_string();
    fs::write(&path, &raw).unwrap();
    let current = super::super::edit_source::Source::load(&path, Some(&raw)).unwrap();
    assert_eq!(source.text, current.text);
    assert!(!native_edit_retry_ready(&ws, &retry));
    assert!(!read().is_error);
    assert!(!native_edit_retry_ready(&ws, &retry));
}

#[test]
fn missing_snapshots_retain_every_files_required_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    for name in ["a.txt", "b.txt"] {
        fs::write(dir.path().join(name), "one\ntwo\n").unwrap();
    }
    let tag = tag_of("one\ntwo\n");
    let patch = format!("[a.txt#{tag}]\nPUT 1.=1:\n+ONE\n[b.txt#{tag}]\nPUT 1.=1:\n+ONE\n");
    let capture = crate::runtime::execution::Capture::begin();
    assert!(
        edit(&ws, &patch)
            .unwrap_err()
            .contains("no read of this file")
    );
    let retry = capture.finish(false).native_edit_retry.unwrap();
    assert_eq!(retry.requirements.len(), 2);
    assert!(!native_edit_retry_ready(&ws, &retry));
    for (path, ready) in [
        ("a.txt:1-1", false),
        ("b.txt:2-2", false),
        ("b.txt:1-1", true),
    ] {
        let read = crate::tools::execute(&ws, "read", &serde_json::json!({"path":path}), 20);
        assert!(!read.is_error, "{}", read.text);
        assert_eq!(native_edit_retry_ready(&ws, &retry), ready, "{path}");
    }
}

#[test]
fn unread_ranges_across_operations_and_files_are_revealed_in_one_retry() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let a = ws.resolve("a.txt").unwrap();
    let b = ws.resolve("b.txt").unwrap();
    let original = "one\ntwo\nthree\nfour\nfive\nsix\n";
    fs::write(&a, original).unwrap();
    fs::write(&b, "other\n").unwrap();
    let tag = ws.store().record(&a, original, &[(1, 1), (3, 3)]);
    let btag = ws.store().record(&b, "other\n", &[]);
    // Reverse anchor order, multiple gaps in one range, and a register whose CUT is unread.
    let patch = format!(
        "[a.txt#{tag}]\nCUT 6.=6 @saved\nPUT 2.=4:\n+replacement\nPUT >1 @saved\n[b.txt#{btag}]\nPUT 1.=1:\n+OTHER\n"
    );
    let capture = crate::runtime::execution::Capture::begin();
    let error = edit(&ws, &patch).unwrap_err();
    let retry = capture.finish(false).native_edit_retry.unwrap();
    assert_eq!(retry.requirements.len(), 2);
    assert!(native_edit_retry_ready(&ws, &retry));
    for row in ["2:two\n", "4:four\n", "6:six\n", "1:other\n"] {
        assert!(error.contains(row), "{error}");
    }
    assert!(!error.contains("5:five"), "{error}");
    assert_eq!(fs::read_to_string(&a).unwrap(), original);
    assert_eq!(fs::read_to_string(&b).unwrap(), "other\n");
    edit(&ws, &patch).unwrap();
    assert_eq!(
        fs::read_to_string(&a).unwrap(),
        "one\nsix\nreplacement\nfive\n"
    );
    assert_eq!(fs::read_to_string(&b).unwrap(), "OTHER\n");
}

#[test]
fn put_from_a_register_cut_later_in_the_same_call_is_refused_not_stale() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let a = ws.resolve("a.txt").unwrap();
    let b = ws.resolve("b.txt").unwrap();
    let c = ws.resolve("c.txt").unwrap();
    fs::write(&a, "one\ntwo\nthree\n").unwrap();
    fs::write(&b, "b\n").unwrap();
    fs::write(&c, "stale\nc\n").unwrap();
    // Fill @x in an earlier call so a stale value exists to be pasted by mistake.
    let ctag = ws.store().record_all(&c, "stale\nc\n");
    edit(&ws, &format!("[c.txt#{ctag}]\nCUT 1.=1 @x\n")).unwrap();
    let original_a = fs::read_to_string(&a).unwrap();
    let tag = ws.store().record_all(&a, &original_a);
    let btag = ws.store().record_all(&b, "b\n");
    let error = edit(
        &ws,
        &format!("[b.txt#{btag}]\nPUT >1 @x\n[a.txt#{tag}]\nCUT 3.=3 @x\n"),
    )
    .unwrap_err();
    assert!(error.contains("put the CUT section first"), "{error}");
    assert_eq!(fs::read_to_string(&a).unwrap(), original_a);
    assert_eq!(fs::read_to_string(&b).unwrap(), "b\n");
    // Authored in CUT-then-PUT order, the same move succeeds.
    edit(
        &ws,
        &format!("[a.txt#{tag}]\nCUT 3.=3 @x\n[b.txt#{btag}]\nPUT >1 @x\n"),
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&b).unwrap(), "b\nthree\n");
}

#[test]
fn invalid_anchors_remain_visible_alongside_unread_previews() {
    for invalid_first in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let path = ws.resolve("a.txt").unwrap();
        let original = "one\ntwo\nthree\n";
        fs::write(&path, original).unwrap();
        let tag = ws.store().record(&path, original, &[(1, 1)]);
        let invalid = "PUT 99.=99:\n+invalid\n";
        let unread = "PUT 2.=2:\n+TWO\n";
        let operations = if invalid_first {
            format!("{invalid}{unread}")
        } else {
            format!("{unread}{invalid}")
        };
        let patch = format!("[a.txt#{tag}]\n{operations}");
        let capture = crate::runtime::execution::Capture::begin();
        let error = edit(&ws, &patch).unwrap_err();
        assert!(capture.finish(false).native_edit_retry.is_none());
        assert!(
            error.contains("line 99 is beyond the end of the file (3 lines)"),
            "{error}"
        );
        let invalid_line = if invalid_first { 2 } else { 4 };
        assert!(
            error.contains(&format!("line {invalid_line}: a.txt#{tag}")),
            "{error}"
        );
        assert!(error.contains("have not been shown by a read"), "{error}");
        assert!(error.contains("2:two\n"), "{error}");
        assert!(!error.contains("3:three"), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(
            ws.store().witness_flags(&path, original),
            vec![true, true, false]
        );
        // Previewing the unread line does not make an unchanged retry valid.
        let retry = edit(&ws, &patch).unwrap_err();
        assert!(
            retry.contains("line 99 is beyond the end of the file"),
            "{retry}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
}

#[test]
fn unread_preview_does_not_grant_witnesses_to_omitted_lines() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("a.txt").unwrap();
    let original = (1..=MISMATCH_MAX_ROWS + 2)
        .map(|n| format!("row {n}\n"))
        .collect::<String>();
    fs::write(&path, &original).unwrap();
    let tag = ws.store().record(&path, &original, &[]);
    let patch = format!(
        "[a.txt#{tag}]\nPUT 1.={}:\n+replacement\n",
        MISMATCH_MAX_ROWS + 2
    );
    let capture = crate::runtime::execution::Capture::begin();
    let error = edit(&ws, &patch).unwrap_err();
    let retry = capture.finish(false).native_edit_retry.unwrap();
    assert!(!native_edit_retry_ready(&ws, &retry));
    assert!(error.contains("preview truncated"), "{error}");
    let flags = ws.store().witness_flags(&path, &original);
    assert!(flags[..MISMATCH_MAX_ROWS].iter().all(|seen| *seen));
    assert!(flags[MISMATCH_MAX_ROWS..].iter().all(|seen| !*seen));
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn stale_unread_anchors_do_not_witness_current_lines() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.resolve("a.txt").unwrap();
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let tag = ws.store().record(&path, "one\ntwo\nthree\n", &[(1, 1)]);
    fs::write(&path, "one\nTWO\nTHREE\n").unwrap();
    let patch = format!("[a.txt#{tag}]\nPUT 2.=2:\n+x\nPUT 3.=3:\n+y\n");
    let capture = crate::runtime::execution::Capture::begin();
    assert!(edit(&ws, &patch).is_err());
    assert!(capture.finish(false).native_edit_retry.is_none());
    assert_eq!(
        ws.store().witness_flags(&path, "one\nTWO\nTHREE\n"),
        vec![false; 3]
    );
    assert!(edit(&ws, &patch).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\nTWO\nTHREE\n");
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
fn empty_put_bodies_and_sections_name_the_corrected_operation() {
    let mid = parse_patch("[a.rs#1234]\nPUT 3.=5:\n\nPUT >7:\n+x\n").unwrap_err();
    assert!(
        mid.contains("line 2: `PUT 3.=5:` needs at least one `+` row"),
        "{mid}"
    );
    assert!(
        mid.contains("write `CUT 3.=5`") && mid.contains("lone `+`"),
        "{mid}"
    );
    let last = parse_patch("[a.rs#1234]\nPUT 4*:\n").unwrap_err();
    assert!(
        last.contains("`PUT 4*:`") && last.contains("`CUT 4*`"),
        "{last}"
    );
    let insert = parse_patch("[a.rs#1234]\nPUT >2:\n").unwrap_err();
    assert!(insert.contains("remove the operation"), "{insert}");
    assert!(!insert.contains("CUT"), "{insert}");
    let empty = parse_patch("[a.rs#1234]\n[b.rs#1234]\nCUT 1.=1\n").unwrap_err();
    assert!(
        empty.contains("has no operations") && empty.contains("remove the header"),
        "{empty}"
    );
}

#[test]
fn an_empty_or_malformed_tag_quotes_the_latest_read_without_applying_the_edit() {
    for patch in [
        "[a.txt#]\nPUT 1.=1:\n+ONE\n",
        "[a.txt#auto]\nPUT 1.=1:\n+ONE\n",
        "[a.txt#12]\nPUT 1.=1:\n+ONE\n",
    ] {
        a_tagless_edit_quotes_the_latest_read(patch);
    }
}

fn a_tagless_edit_quotes_the_latest_read(patch: &str) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let path = ws.root().join("a.txt");
    fs::write(&path, "one\n").unwrap();
    let unread = edit(&ws, patch).unwrap_err();
    assert!(
        unread.contains("no read of this file is recorded"),
        "{unread}"
    );

    let tag = ws.store().record_all(&path, "one\n");
    let fresh = edit(&ws, patch).unwrap_err();
    assert!(fresh.contains(&format!("[a.txt#{tag}]")), "{fresh}");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "one\n",
        "never resolved silently"
    );

    fs::write(&path, "changed\n").unwrap();
    let stale = edit(&ws, patch).unwrap_err();
    assert!(
        stale.contains(&format!("#{tag}")) && stale.contains(&tag_of("changed\n")),
        "{stale}"
    );
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

#[test]
fn resume_rehydrates_witnesses_from_a_multi_path_read() {
    // Benchmark follow-up turns run in a new process: a `paths` read from the previous turn must
    // still count as read, or the first edit is refused with "no read of this file is recorded".
    let dir = tempfile::tempdir().unwrap();
    let first = Workspace::new(dir.path()).unwrap();
    fs::create_dir(dir.path().join("pkg")).unwrap();
    fs::write(dir.path().join("a.py"), "x = 1\ny = 2\n").unwrap();
    fs::write(dir.path().join("b.py"), "def f():\n    return 1\n").unwrap();
    let args = serde_json::json!({"paths": ["pkg", "a.py", "b.py:1-1"]});
    let read = crate::tools::execute(&first, "read", &args, 20);
    assert!(!read.is_error, "{}", read.text);
    let tag_a = tag_of("x = 1\ny = 2\n");
    let tag_b = tag_of("def f():\n    return 1\n");

    let resumed = Workspace::new(dir.path()).unwrap();
    let refused = edit(&resumed, &format!("[a.py#{tag_a}]\nPUT 2.=2:\n+y = 3\n")).unwrap_err();
    assert!(
        refused.contains("no read of this file is recorded")
            && refused.contains("a.py:START-END")
            && refused.contains("fresh [path#TAG] header"),
        "{refused}"
    );
    rehydrate(&resumed, [("read", &args, read.text.as_str())]);
    edit(&resumed, &format!("[a.py#{tag_a}]\nPUT 2.=2:\n+y = 3\n")).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("a.py")).unwrap(),
        "x = 1\ny = 3\n"
    );
    // Only the rows that read returned are witnesses: line 2 of b.py was never shown.
    let unseen = edit(
        &resumed,
        &format!("[b.py#{tag_b}]\nPUT 2.=2:\n+    return 2\n"),
    )
    .unwrap_err();
    assert!(unseen.contains("have not been shown"), "{unseen}");
    assert_eq!(
        fs::read_to_string(dir.path().join("b.py")).unwrap(),
        "def f():\n    return 1\n"
    );
}

#[test]
fn check_version_ranges_names_the_unseen_span_and_the_targeted_retry() {
    let mut store = EditStore::default();
    let path = Path::new("/w/a.txt");
    let lf = "one\ntwo\nthree\n";
    store.record(path, lf, &[(1, 1)]);
    store
        .check_version_ranges(path, lf, None, &[(1, 1)])
        .unwrap();
    let err = store
        .check_version_ranges(path, lf, None, &[(1, 3)])
        .unwrap_err();
    assert!(
        err.contains("/w/a.txt lines 2-3 were not displayed in full"),
        "{err}"
    );
    assert!(err.contains("read `/w/a.txt:2-3` first"), "{err}");
    assert!(
        err.contains("for a hashline edit, retry with the fresh [path#TAG] header"),
        "{err}"
    );
    assert!(
        err.contains("otherwise retry the edit after the refreshed read"),
        "{err}"
    );
}

#[test]
fn check_version_ranges_without_a_matching_witness_asks_for_a_targeted_read() {
    let mut store = EditStore::default();
    let path = Path::new("/w/a.txt");
    let lf = "one\ntwo\n";
    store.record(path, lf, &[(1, 2)]);
    // Different text, and the same text under a different revision, both lack a witness.
    for (text, revision) in [("other\n", None), (lf, Some([7u8; 32]))] {
        let err = store
            .check_version_ranges(path, text, revision, &[(1, 1)])
            .unwrap_err();
        assert!(
            err.contains("/w/a.txt has no current source/read witness"),
            "{err}"
        );
        assert!(
            err.contains("targeted read such as `/w/a.txt:START-END`"),
            "{err}"
        );
        assert!(
            err.contains("for a hashline edit, retry with the fresh [path#TAG] header"),
            "{err}"
        );
        assert!(
            err.contains("otherwise retry the edit after the refreshed read"),
            "{err}"
        );
    }
    let unknown = Path::new("/w/never.txt");
    let err = store
        .check_version_ranges(unknown, lf, None, &[(1, 1)])
        .unwrap_err();
    assert!(
        err.contains("/w/never.txt has no current source/read witness"),
        "{err}"
    );
}

#[test]
fn unread_and_stale_headers_direct_a_targeted_read_and_fresh_header() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    fs::write(dir.path().join("a.py"), "x = 1\ny = 2\n").unwrap();
    fs::write(dir.path().join("b.py"), "p\n").unwrap();
    // Never read: the header matches the current file, so the no-read branch fires.
    let unread = edit(
        &ws,
        &format!("[a.py#{}]\nPUT 2.=2:\n+y = 3\n", tag_of("x = 1\ny = 2\n")),
    )
    .unwrap_err();
    assert!(
        unread.contains("no read of this file is recorded"),
        "{unread}"
    );
    assert!(unread.contains("a.py:START-END"), "{unread}");
    assert!(unread.contains("fresh [path#TAG] header"), "{unread}");
    // Read b.py, then send a header for other content: the header is stale.
    let args = serde_json::json!({"paths": ["b.py"]});
    let read = crate::tools::execute(&ws, "read", &args, 20);
    assert!(!read.is_error, "{}", read.text);
    let stale = edit(
        &ws,
        &format!("[b.py#{}]\nPUT 1.=1:\n+q\n", tag_of("other\n")),
    )
    .unwrap_err();
    assert!(stale.contains("unknown snapshot tag"), "{stale}");
    assert!(stale.contains("b.py:START-END"), "{stale}");
    assert!(stale.contains("fresh [path#TAG] header"), "{stale}");
    assert_eq!(fs::read_to_string(dir.path().join("b.py")).unwrap(), "p\n");
}

#[test]
fn a_wide_search_page_cannot_evict_the_files_the_model_read() {
    let mut store = EditStore::default();
    let read = std::path::PathBuf::from("/w/read.rs");
    store.record_all(&read, "fn read() {}\n");
    // One grep page touching more files than the store holds.
    for index in 0..MAX_FILES + 50 {
        store.record_search(
            &std::path::PathBuf::from(format!("/w/hit{index}.rs")),
            "needle\n",
            &[(1, 1)],
        );
    }
    assert!(store.files.contains_key(&read), "the read witness survives");
    // Even a store full of read witnesses keeps them all when a search hit arrives.
    let mut full = EditStore::default();
    for index in 0..MAX_FILES {
        full.record_all(
            &std::path::PathBuf::from(format!("/w/read{index}.rs")),
            "x\n",
        );
    }
    full.record_search(
        &std::path::PathBuf::from("/w/hit.rs"),
        "needle\n",
        &[(1, 1)],
    );
    assert!(
        full.files
            .contains_key(&std::path::PathBuf::from("/w/read0.rs"))
    );
    assert!(store.lru.len() <= MAX_FILES);
    // Searching a file the model already read refreshes it like any other observation.
    store.record_search(&read, "fn read() {}\n", &[(1, 1)]);
    assert_eq!(store.lru.last(), Some(&read));
}

#[test]
fn an_oversized_first_line_is_clipped_instead_of_reported_as_an_empty_range() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let long = "x".repeat(3000);
    let text = format!("a\n{long}\nb\nc\n");
    std::fs::write(dir.path().join("f.rs"), &text).unwrap();
    let budget = crate::tools::fs::select::READ_FOOTER_RESERVE + 500;
    let out = read_navigation(&ws, "f.rs", 2, 3, budget).unwrap();
    assert!(!out.contains("no lines in that range"), "{out}");
    assert!(out.contains("\n2:xxx"), "{out}");
    assert!(out.contains("…[line truncated]"), "{out}");
    assert!(out.contains("continue with `f.rs:3-`"), "{out}");
    // A clipped row is not an edit witness.
    let abs = ws.resolve("f.rs").unwrap();
    assert!(!ws.store().witness_flags(&abs, &text)[1]);
    // With no room for even a clipped row, the result says why rather than "empty range".
    let tiny = read_navigation(
        &ws,
        "f.rs",
        2,
        3,
        crate::tools::fs::select::READ_FOOTER_RESERVE + 10,
    )
    .unwrap();
    assert!(
        tiny.contains("line 2 does not fit the remaining output budget"),
        "{tiny}"
    );
}

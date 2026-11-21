use super::*;
thread_local! { pub(super) static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[test]
fn invalid_source_has_advisory_diagnostics_not_a_write_veto() {
    assert!(advisory(Path::new("a.rs"), "fn a() {}", "fn a( {").is_some());
    assert!(advisory(Path::new("a.rs"), "fn a( {", "fn a() {}").is_none());
    assert_eq!(
        report(Path::new("notes.txt"), "fn a( {")["supported"],
        false
    );
}

#[test]
fn a_syntax_refusal_says_nothing_landed_and_names_its_new_names() {
    let before = "WIDTH = 10\n\ndef draw(self):\n    pass\n";
    let after = "WIDTH = 10\nSIDEBAR_WIDTH = 5\n\ndef draw(self    :\n    pass\n";
    let refused = refusal(Path::new("a.py"), "a.py", before, after).expect("refused");
    assert!(
        refused.contains("Nothing from this edit exists in the file")
            && refused.contains("`SIDEBAR_WIDTH`")
            && !refused.contains("`draw`")
            && refused.contains("Fix the edit text and retry"),
        "{refused}"
    );
}

#[test]
fn a_large_syntax_refusal_suggests_smaller_edits_or_write() {
    let lines = |range: std::ops::Range<usize>| -> String {
        range
            .map(|n| format!("def f{n}():\n    return {n}\n"))
            .collect()
    };
    let before = lines(0..100);
    // Rewrite 50 of 100 functions with a syntax error: split into smaller edits.
    let block = lines(20..70);
    let broken = block.replace("def f30():", "def f30(:");
    let after = before.replace(&block, &broken.replace("return", "yield"));
    let refused = refusal(Path::new("a.py"), "a.py", &before, &after).expect("refused");
    assert!(refused.contains("split it into smaller edits"), "{refused}");
    assert!(!refused.contains("`write`"), "{refused}");
    // Rewriting the whole file: send it once with write.
    let whole = before
        .replace("return", "yield")
        .replace("def f0():", "def f0(:");
    let refused = refusal(Path::new("a.py"), "a.py", &before, &whole).expect("refused");
    assert!(refused.contains("`write`"), "{refused}");
}

#[test]
fn python_short_strings_reject_literal_newlines_but_not_valid_multiline_forms() {
    let path = Path::new("game.py");
    for source in [
        "print(\"\n\")\n",
        "print('before\nafter')\n",
        "print(r\"before\nafter\")\n",
        "print(f\"before\nafter\")\n",
        "print(\"before\\\\\nafter\")\n",
    ] {
        assert_eq!(report(path, source)["valid"], false, "{source:?}");
        assert!(
            refusal(path, "game.py", "print()\n", source).is_some(),
            "{source:?}"
        );
    }
    for source in [
        "print(\"\\n\")\n",
        "print(\"before\\\nafter\")\n",
        "print(\"before\\\r\nafter\")\n",
        "print(r\"before\\\nafter\")\n",
        "print('''before\nafter''')\n",
        "print(f\"\"\"before\n{1} after\"\"\")\n",
        "print(f\"{(\n1 + 2\n)}\")\n",
        "# not a string: \"\nprint('洞窟')\n",
    ] {
        assert_eq!(report(path, source)["valid"], true, "{source:?}");
        assert!(
            refusal(path, "game.py", "print()\n", source).is_none(),
            "{source:?}"
        );
    }
}

#[test]
fn local_repair_respects_witness_and_leaves_advisory_proposals_intact_on_abstention() {
    let (_dir, mut ws) = crate::tools::testkit::workspace();
    ws.edit_settings.auto_repair.enabled = true;
    let path = ws.root().join("values.py");
    let no_coach = |_: &crate::tools::EditRepairRequest| -> Result<String, String> {
        panic!("unexpected coach")
    };
    for (before, proposed, flags) in [
        ("v = f(1)", "v = f(2", &[false][..]),
        ("v = f(1", "v = f(2", &[true]),
        ("v = f(1)", "v = f(\n    2\n)", &[true, true, true]),
        ("# )\nv = f(1)", "# ]\nv = f(2", &[true, true]),
    ] {
        let source = prepare_edit(
            &ws,
            &path,
            before,
            proposed,
            Some(flags),
            Some(&no_coach),
            false,
        )
        .unwrap();
        assert_eq!(source.text, proposed);
    }
}

#[test]
fn only_would_refuse_edits_pay_for_a_repair_candidate() {
    let (_dir, ws) = crate::tools::testkit::workspace();
    let path = ws.root().join("values.py");
    for enabled in [false, true] {
        for (guard, block, src, count, expected, advisory) in [
            (false, false, "v = f(2", 0, Some("v = f(2"), false),
            (true, false, "v = f(2", 2, Some("v = f(2"), true),
            (true, true, "v = f(2)", 1, Some("v = f(2)"), false),
            (
                true,
                true,
                "v = f(2",
                if enabled { 3 } else { 2 },
                enabled.then_some("v = f(2)"),
                false,
            ),
        ] {
            let mut ws = ws.clone().with_syntax_guard(guard);
            ws.edit_settings.auto_repair.enabled = enabled;
            PARSES.set(0);
            let result = prepare_edit(&ws, &path, "v = f(1)", src, Some(&[true]), None, block);
            assert_eq!(PARSES.get(), count, "{enabled}/{guard}/{block}/{src}");
            assert_eq!(result.as_ref().ok().map(|v| v.text.as_str()), expected);
            if let Ok(value) = result {
                assert_eq!(!value.warnings.is_empty(), advisory);
            }
        }
    }
    let mut ws = ws;
    ws.edit_settings.auto_repair.enabled = true;
    assert!(
        prepare_edit(
            &ws,
            &path,
            "v = f(1)",
            "v = f(2",
            Some(&[false]),
            None,
            true
        )
        .is_err()
    );
}

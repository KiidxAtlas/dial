use super::*;
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

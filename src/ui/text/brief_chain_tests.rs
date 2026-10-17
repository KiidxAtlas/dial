use super::brief_chain;

#[test]
fn long_anyhow_chains_keep_the_first_two_clauses_and_the_root_cause() {
    let chain = "Selected memory recall unavailable: opening the memory store failed while refreshing the index for this project: sqlite returned an error for the statement: database is locked";
    assert_eq!(
        brief_chain(chain),
        "Selected memory recall unavailable: opening the memory store failed while refreshing the index for this project … database is locked"
    );
}

#[test]
fn short_text_colons_and_multiline_text_are_untouched() {
    for text in [
        "Verification build: Passed",
        "a: b: c: d",
        "Changed files this turn: 2\n  src/a.rs: new\n  src/b.rs: edited\n  and more lines to pass the length threshold of this check comfortably",
    ] {
        assert_eq!(brief_chain(text), text);
    }
}

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

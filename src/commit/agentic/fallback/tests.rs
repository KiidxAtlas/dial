use super::super::lock_files::is_lock_file;
use super::*;

fn entry(path: &str) -> NumstatEntry {
    NumstatEntry {
        path: path.to_owned(),
        additions: Some(1),
        deletions: Some(0),
    }
}

#[test]
fn test_only_changes_infer_test_type() {
    let analysis = generate_fallback_analysis(&[entry("tests/a_test.rs")]);
    assert_eq!(analysis.commit_type, "test");
}

#[test]
fn top_level_test_directory_infers_test_type() {
    let analysis = generate_fallback_analysis(&[entry("tests/helpers.rs")]);
    assert_eq!(analysis.commit_type, "test");
}

#[test]
fn mixed_source_infers_refactor() {
    let analysis = generate_fallback_analysis(&[entry("src/lib.rs"), entry("README.md")]);
    assert_eq!(analysis.commit_type, "refactor");
}

#[test]
fn summary_counts_other_files() {
    let numstat = [entry("a/x.rs"), entry("b/y.rs"), entry("c/z.rs")];
    assert_eq!(
        generate_fallback_summary("refactor", &numstat),
        "refactored x.rs and 2 others"
    );
    assert_eq!(
        generate_fallback_summary("docs", &numstat[..1]),
        "updated documentation for x.rs"
    );
}

#[test]
fn proposal_carries_fallback_warning() {
    let proposal = generate_fallback_proposal(&[entry("Cargo.lock")]);
    assert_eq!(proposal.warnings, vec![FALLBACK_WARNING.to_owned()]);
    assert!(is_lock_file("Cargo.lock"));
}

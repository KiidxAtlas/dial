use super::super::state::{SplitCommitGroup, SplitCommitPlan};
use super::*;

fn commit(paths: &[&str]) -> SplitCommitGroup {
    SplitCommitGroup {
        changes: paths
            .iter()
            .map(|path| FileChange::All {
                path: (*path).to_owned(),
            })
            .collect(),
        commit_type: "chore".to_owned(),
        scope: None,
        summary: "Updated deps".to_owned(),
        details: Vec::new(),
        issue_refs: Vec::new(),
        rationale: None,
        dependencies: Vec::new(),
    }
}

#[test]
fn lock_file_joins_manifest_commit_in_same_directory() {
    let mut plan = SplitCommitPlan {
        commits: vec![commit(&["src/a.rs"]), commit(&["crates/x/Cargo.toml"])],
        warnings: Vec::new(),
    };
    let staged = vec![
        "src/a.rs".to_owned(),
        "crates/x/Cargo.toml".to_owned(),
        "crates/x/Cargo.lock".to_owned(),
    ];
    assign_lock_files_to_plan(&mut plan, &staged);
    assert!(
        plan.commits[1]
            .changes
            .iter()
            .any(|c| c.path() == "crates/x/Cargo.lock")
    );
}

#[test]
fn orphan_lock_file_falls_back_to_last_commit() {
    let mut plan = SplitCommitPlan {
        commits: vec![commit(&["a.rs"]), commit(&["b.rs"])],
        warnings: Vec::new(),
    };
    let staged = vec!["a.rs".to_owned(), "b.rs".to_owned(), "yarn.lock".to_owned()];
    assign_lock_files_to_plan(&mut plan, &staged);
    assert!(
        plan.commits[1]
            .changes
            .iter()
            .any(|c| c.path() == "yarn.lock")
    );
}

#[test]
fn non_lock_files_are_ignored() {
    let mut plan = SplitCommitPlan {
        commits: vec![commit(&["a.rs"])],
        warnings: Vec::new(),
    };
    assign_lock_files_to_plan(&mut plan, &["a.rs".to_owned(), "other.txt".to_owned()]);
    assert_eq!(plan.commits[0].changes.len(), 1);
}

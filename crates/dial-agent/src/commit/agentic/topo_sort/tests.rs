use super::super::state::SplitCommitGroup;
use super::*;

fn group(dependencies: Vec<usize>) -> SplitCommitGroup {
    SplitCommitGroup {
        changes: Vec::new(),
        commit_type: "feat".to_owned(),
        scope: None,
        summary: "Added thing".to_owned(),
        details: Vec::new(),
        issue_refs: Vec::new(),
        rationale: None,
        dependencies,
    }
}

#[test]
fn dependencies_come_first() {
    // commit 0 depends on 2; commit 2 depends on 1.
    let groups = vec![group(vec![2]), group(vec![]), group(vec![1])];
    let order = compute_dependency_order(&groups).expect("acyclic plan");
    let pos = |i: usize| order.iter().position(|&x| x == i).expect("present");
    assert!(pos(1) < pos(2));
    assert!(pos(2) < pos(0));
    assert_eq!(order.len(), 3);
}

#[test]
fn cycle_is_reported() {
    let groups = vec![group(vec![1]), group(vec![0])];
    assert_eq!(
        compute_dependency_order(&groups).unwrap_err(),
        "Circular dependency detected in split commit plan."
    );
}

#[test]
fn out_of_range_dependency_is_reported() {
    let groups = vec![group(vec![5])];
    assert_eq!(
        compute_dependency_order(&groups).unwrap_err(),
        "Invalid dependency index: 5"
    );
}

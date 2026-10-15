//! Dependency ordering for split commit plans.

use std::collections::VecDeque;

use super::state::SplitCommitGroup;

/// Kahn's algorithm over the `dependencies` edges of `groups`.
///
/// Returns commit indices in an order where every dependency precedes its dependents. Errors on
/// out-of-range dependencies and on cycles, with fixed error messages.
pub(crate) fn compute_dependency_order(groups: &[SplitCommitGroup]) -> Result<Vec<usize>, String> {
    let total = groups.len();
    let mut in_degree = vec![0usize; total];
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); total];

    for (index, group) in groups.iter().enumerate() {
        for &dependency in &group.dependencies {
            if dependency >= total {
                return Err(format!("Invalid dependency index: {dependency}"));
            }
            if !edges[dependency].contains(&index) {
                edges[dependency].push(index);
                in_degree[index] += 1;
            }
        }
    }

    let mut queue: VecDeque<usize> = (0..total).filter(|&index| in_degree[index] == 0).collect();
    let mut order = Vec::with_capacity(total);
    while let Some(current) = queue.pop_front() {
        order.push(current);
        for &next in &edges[current] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                queue.push_back(next);
            }
        }
    }

    if order.len() != total {
        return Err("Circular dependency detected in split commit plan.".to_owned());
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
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
}

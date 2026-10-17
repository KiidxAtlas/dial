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
mod tests;

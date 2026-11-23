//! Read-only comparison of recorded call ordering against current workspace resource contracts.
use crate::agent::run::dependency_dispatch::{WavePolicy, WaveScope, plan_wave};
use crate::{
    config::types::Config,
    llm::ToolCall,
    tools::{Workspace, integrations::ext::Registry},
};
use serde::Serialize;
use std::collections::BTreeMap;

/// Structural admission opportunities, not executed work or predicted wall-clock savings.
/// Historical filesystem state, live approvals, controller preflights and provider streaming
/// times cannot be reconstructed from an assistant call list.
#[derive(Default, Debug, Serialize)]
pub struct SchedulingReplay {
    pub responses: usize,
    pub calls: usize,
    pub read_batching_only: ReplayGroups,
    pub resource_dispatch: ReplayGroups,
    pub resource_barriers: BTreeMap<String, usize>,
    /// Barriers closed by a specific call, keyed `reason:tool` (see `Scheduling::barrier_tools`).
    pub resource_barrier_tools: BTreeMap<String, usize>,
}

impl SchedulingReplay {
    /// Combine reports from separate sessions, each replayed with its own file handles.
    pub fn extend(&mut self, other: Self) {
        self.responses += other.responses;
        self.calls += other.calls;
        self.read_batching_only.extend(other.read_batching_only);
        self.resource_dispatch.extend(other.resource_dispatch);
        for (key, count) in other.resource_barriers {
            *self.resource_barriers.entry(key).or_default() += count;
        }
        for (key, count) in other.resource_barrier_tools {
            *self.resource_barrier_tools.entry(key).or_default() += count;
        }
    }
}

#[derive(Default, Debug, Serialize)]
pub struct ReplayGroups {
    pub groups: usize,
    pub grouped_calls: usize,
    pub sizes: BTreeMap<usize, usize>,
}
impl ReplayGroups {
    fn group(&mut self, count: usize) {
        self.groups += 1;
        if count > 1 {
            self.grouped_calls += count;
        }
        *self.sizes.entry(count).or_default() += 1;
    }
    fn extend(&mut self, other: Self) {
        self.groups += other.groups;
        self.grouped_calls += other.grouped_calls;
        for (size, count) in other.sizes {
            *self.sizes.entry(size).or_default() += count;
        }
    }
}

/// Uses the execution planner and tool-owned resource declarations without invoking any tool.
/// Calls are expanded through `workspace`'s file-handle registry exactly as the live wave does, so
/// restore the recorded session's handles first; an unknown handle is a `file_reference` barrier.
pub fn replay_scheduling(
    workspace: &Workspace,
    config: &Config,
    registry: &Registry,
    responses: &[Vec<ToolCall>],
) -> SchedulingReplay {
    let mut report = SchedulingReplay::default();
    for calls in responses.iter().filter(|calls| !calls.is_empty()) {
        report.responses += 1;
        report.calls += calls.len();
        for resource_dispatch in [false, true] {
            let mut index = 0;
            while index < calls.len() {
                let rest = &calls[index..];
                let mut count = 0;
                if resource_dispatch {
                    let scope = WaveScope {
                        workspace,
                        mode: workspace.edit_mode(),
                        registry,
                        hooks: &config.hooks,
                        limit: config.task.dependency_wave_limit(),
                    };
                    let plan = plan_wave(rest, scope, WavePolicy::PARENT, |call| {
                        if call.malformed_arguments {
                            return Err("malformed_arguments");
                        }
                        let mut effective = call.clone();
                        crate::agent::window::file_handles::expand(workspace, &mut effective)
                            .map_err(|_| "file_reference")?;
                        Ok(effective)
                    });
                    *report
                        .resource_barriers
                        .entry(plan.barrier.into())
                        .or_default() += 1;
                    if let Some(tool) = &plan.barrier_tool {
                        *report
                            .resource_barrier_tools
                            .entry(format!("{}:{tool}", plan.barrier))
                            .or_default() += 1;
                    }
                    if plan.dispatchable() {
                        count = plan.calls.len();
                    }
                }
                if count == 0 {
                    count = rest
                        .iter()
                        .take(crate::agent::run::dispatch::MAX_BATCH_ENTRIES)
                        .take_while(|call| {
                            !call.malformed_arguments
                                && ["pre_tool_use", "post_tool_use"].iter().all(|event| {
                                    !crate::agent::safety::hooks::wanted_for_tool(
                                        &config.hooks,
                                        event,
                                        &call.name,
                                    )
                                })
                                && crate::agent::safety::approval::parallel_read(
                                    registry,
                                    &call.name,
                                    &call.arguments,
                                )
                        })
                        .count()
                        .max(1);
                }
                if resource_dispatch {
                    report.resource_dispatch.group(count);
                } else {
                    report.read_batching_only.group(count);
                }
                index += count;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests;

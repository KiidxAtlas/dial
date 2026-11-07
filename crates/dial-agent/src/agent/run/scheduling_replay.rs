//! Read-only comparison of recorded call ordering against current workspace resource contracts.
use crate::agent::run::dependency_dispatch::{bookkeeping, footprint_for, plan_prefix};
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
}

/// Uses the execution planner and tool-owned resource declarations without invoking any tool.
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
                    let plan = plan_prefix(rest, config.task.dependency_wave_limit(), |call| {
                        if call.malformed_arguments {
                            return Err("malformed_arguments");
                        }
                        if ["pre_tool_use", "post_tool_use"].iter().any(|event| {
                            crate::agent::safety::hooks::wanted_for_tool(
                                &config.hooks,
                                event,
                                &call.name,
                            )
                        }) {
                            return Err("hook_or_preflight");
                        }
                        let resources =
                            footprint_for(workspace, workspace.edit_mode(), registry, call)
                                .ok_or("unknown_resources")?;
                        Ok((call.clone(), resources))
                    });
                    *report
                        .resource_barriers
                        .entry(plan.barrier.into())
                        .or_default() += 1;
                    let native = plan.calls.iter().filter(|call| !bookkeeping(call)).count();
                    if native > 1 && (plan.writes || plan.calls.iter().any(bookkeeping)) {
                        count = plan.calls.len();
                    }
                }
                if count == 0 {
                    count = rest
                        .iter()
                        .take(config.task.parallel_read_limit())
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

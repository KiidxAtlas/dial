//! Conservative native resource waves. Unknown effects are barriers, never model-declared leases.
use crate::agent::*;

pub(in crate::agent) use crate::tools::surface::resources::Footprint;

/// Bookkeeping is settled on the controller in call order after native work drains. Deliverable
/// scope changes can ask the human, so they remain boundaries before later source effects.
pub(in crate::agent) fn bookkeeping(call: &ToolCall) -> bool {
    call.name == "todo"
        || (call.name == "working_state"
            && (call.arguments["action"] == "view"
                || (call.arguments["action"] == "patch"
                    && call.arguments.get("deliverables").is_none())))
}

pub(in crate::agent) fn footprint_for(
    workspace: &Workspace,
    mode: crate::config::types::EditMode,
    registry: &crate::tools::integrations::ext::Registry,
    call: &ToolCall,
) -> Option<Footprint> {
    if bookkeeping(call) {
        return Some(Footprint::default());
    }
    crate::tools::surface::resources::resolve(
        workspace,
        mode,
        registry,
        &call.name,
        &call.arguments,
    )
}

/// A conservative prefix shared by foreground, child and recorded-call analysis.
/// Callers retain their policy owners; a rejected call always closes the prefix.
pub(in crate::agent) struct Plan {
    pub calls: Vec<ToolCall>,
    pub writes: bool,
    pub barrier: &'static str,
}

pub(in crate::agent) fn plan_prefix(
    calls: &[ToolCall],
    limit: usize,
    mut admit: impl FnMut(&ToolCall) -> std::result::Result<(ToolCall, Footprint), &'static str>,
) -> Plan {
    let mut plan = Plan {
        calls: Vec::new(),
        writes: false,
        barrier: "response_end",
    };
    let mut footprints = Vec::new();
    for call in calls {
        if plan.calls.len() >= limit {
            plan.barrier = "concurrency_limit";
            break;
        }
        let (call, resources) = match admit(call) {
            Ok(admitted) => admitted,
            Err(reason) => {
                plan.barrier = reason;
                break;
            }
        };
        if footprints
            .iter()
            .any(|previous| resources.conflicts(previous))
        {
            plan.barrier = "resource_conflict";
            break;
        }
        plan.writes |= resources.writes();
        footprints.push(resources);
        let verifies = call.arguments["verify"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty());
        plan.calls.push(call);
        if verifies {
            plan.barrier = "verification";
            break;
        }
    }
    plan
}

#[cfg(test)]
fn footprint(
    workspace: &Workspace,
    mode: crate::config::types::EditMode,
    call: &ToolCall,
) -> Option<Footprint> {
    footprint_for(
        workspace,
        mode,
        &crate::tools::integrations::ext::Registry::new(),
        call,
    )
}

/// Failed native mutations are prerequisites only for actions with overlapping or unknown
/// resources. Diagnostic reads and proven-disjoint native work remain available in this batch.
#[derive(Default)]
pub(in crate::agent) struct BatchPrerequisites {
    failed: Vec<(String, Option<Footprint>)>,
}

impl BatchPrerequisites {
    pub(in crate::agent) fn observe(
        &mut self,
        workspace: &Workspace,
        mode: crate::config::types::EditMode,
        call: &ToolCall,
        output: &ToolOutput,
        registry: &crate::tools::integrations::ext::Registry,
    ) {
        let resources = footprint_for(workspace, mode, registry, call);
        if output.is_error
            && output.execution.status != Some(tools::ToolStatus::Skipped)
            && (matches!(call.name.as_str(), "write" | "edit" | "ast_edit")
                || resources.as_ref().is_some_and(Footprint::writes))
        {
            self.failed.push((call.name.clone(), resources));
        }
    }

    pub(in crate::agent) fn rejection(
        &self,
        workspace: &Workspace,
        mode: crate::config::types::EditMode,
        registry: &crate::tools::integrations::ext::Registry,
        call: &ToolCall,
    ) -> Option<ToolOutput> {
        if self.failed.is_empty() {
            return None;
        }
        // Checks consume the candidate source even though their own approval gate is read-only.
        // Job observations and cancellation do not start work against that candidate.
        if (call.name == "bg"
            && matches!(
                call.arguments["action"].as_str(),
                Some("output" | "wait" | "list" | "kill")
            ))
            || (call.name != "verify"
                && crate::agent::safety::approval::is_read_only(
                    registry,
                    &call.name,
                    &call.arguments,
                ))
        {
            return None;
        }
        let resources = footprint_for(workspace, mode, registry, call);
        let (name, _) =
            self.failed
                .iter()
                .find(|(_, failed)| match (failed.as_ref(), resources.as_ref()) {
                    (Some(failed), Some(resources)) => failed.conflicts(resources),
                    _ => true,
                })?;
        Some(ToolOutput::err(format!(
            "This call was not executed: an earlier `{name}` mutation failed in this batch and this action has overlapping or unknown source dependencies. Inspect the failure and current source, then replan in the next request. Diagnostic reads and proven-disjoint native actions remain available; completed results are preserved."
        ))
        .with_status(tools::ToolStatus::Skipped)
        .with_effects(tools::surface::outcome::Effects::None))
    }
}

impl Agent<'_> {
    /// A maximal independent prefix, bounded by `TaskConfig::dependency_wave_limit` (at most 32).
    /// A conflicting resource or unknown effect closes the wave; nothing jumps a serial barrier.
    pub(in crate::agent) fn dependency_wave(&mut self, calls: &[ToolCall]) -> Vec<ToolCall> {
        let plan = plan_prefix(calls, self.config.task.dependency_wave_limit(), |call| {
            if ["pre_tool_use", "post_tool_use"].iter().any(|event| {
                crate::agent::safety::hooks::wanted_for_tool(self.tool_hooks(), event, &call.name)
            }) || self.repro_first_fires(call, crate::config::types::ReproFirst::Gate)
                || self.speculative_pending(call)
            {
                return Err("hook_or_preflight");
            }
            let mut effective = call.clone();
            crate::agent::window::file_handles::expand(&self.workspace, &mut effective)
                .map_err(|_| "file_reference")?;
            let resources = footprint_for(&self.workspace, self.edit_mode(), &self.ext, &effective)
                .ok_or("unknown_resources")?;
            Ok((effective, resources))
        });
        self.efficiency_metrics.scheduling.barrier(plan.barrier);
        plan.calls
    }

    /// Schema-admitted, dependency-independent native calls use the ordinary worker pump.
    pub(in crate::agent) fn dispatch_dependency_wave(
        &mut self,
        backend: &mut Backend,
        calls: &[ToolCall],
        ui: &mut dyn Ui,
        read_reuse: Option<&crate::agent::workflow::investigation::ReadReuse>,
    ) -> Vec<(ToolOutput, f64)> {
        let mut results = vec![None; calls.len()];
        let blocked = |reason: &str| {
            ToolOutput::err(reason)
                .with_status(tools::ToolStatus::Blocked)
                .with_effects(tools::surface::outcome::Effects::None)
        };
        let started = Instant::now();
        let mut ready = Vec::new();
        let mut positions = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if bookkeeping(call) {
                continue;
            }
            if self.interrupt.load(Ordering::SeqCst) {
                results[index] = Some((crate::tools::exec::execution::before_start(), 0.0));
            } else if let Err(denial) = self.authorize_call(call, ui) {
                results[index] = Some((blocked(denial.message()), 0.0));
            } else if let Some(preview) = self.authorized_edit_preview(call) {
                // Nothing in this wave has started. Preserve the serial preview refusal boundary
                // rather than publishing any sibling effect after a failed authorized preview.
                for result in &mut results {
                    if result.is_none() {
                        *result = Some((
                            blocked(
                                "Native wave not started: an authorized edit preview failed; regenerate the edit.",
                            ),
                            0.0,
                        ));
                    }
                }
                results[index] = Some((preview, 0.0));
                self.edit_preview_intercepted = true;
                return results
                    .into_iter()
                    .map(|result| result.expect("refused wave closed"))
                    .collect();
            } else if results[index].is_none() {
                positions.push(index);
                ready.push(call.clone());
            }
        }
        if !ready.is_empty() {
            let outputs = self
                .run_background_tools(&ready, ui, Some(backend), read_reuse)
                .expect("admitted native tools");
            for (position, output) in positions.into_iter().zip(outputs) {
                results[position] = Some(output);
            }
        }
        // All wave resources are disjoint. Settle each path's observed revision before the first
        // result can be published; the ordinary round still accounts for every call separately.
        if let Some(checkpoints) = &self.checkpoints {
            checkpoints.settle_changes();
        }
        // Settle source epochs before any coalesced verification sees the final candidate tree.
        for (call, output) in calls.iter().zip(&results) {
            if let Some((output, _)) = output {
                self.settle_tool_mutation(call, output);
            }
        }
        for (index, call) in calls.iter().enumerate() {
            if bookkeeping(call) {
                results[index] = Some((self.dispatch(backend, call, ui), 0.0));
            } else if let Some((output, _)) = &mut results[index] {
                self.post_mutation_verification(call, output, ui);
            }
        }
        let results: Vec<_> = results
            .into_iter()
            .map(|result| result.expect("one settled outcome per admitted call"))
            .collect();
        self.efficiency_metrics.scheduling.group(
            "native",
            calls.iter().filter(|call| !bookkeeping(call)).count(),
            started.elapsed(),
            results.iter().map(|(_, seconds)| *seconds),
        );
        for (call, (output, _)) in calls.iter().zip(&results) {
            if !bookkeeping(call) {
                crate::agent::modes::lifecycle::Lifecycle::dispatched(self, call, output);
            }
        }
        results
    }
}

#[cfg(test)]
pub(super) mod tests;

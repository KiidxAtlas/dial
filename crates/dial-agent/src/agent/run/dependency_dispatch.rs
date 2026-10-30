//! Conservative native resource waves. Unknown effects are barriers, never model-declared leases.
use crate::agent::*;

#[derive(Clone, Debug)]
pub(in crate::agent) struct Footprint {
    reads: Vec<PathBuf>,
    writes: Vec<PathBuf>,
}

impl Footprint {
    fn conflicts(&self, other: &Self) -> bool {
        let overlap = |a: &PathBuf, b: &PathBuf| a.starts_with(b) || b.starts_with(a);
        self.writes.iter().any(|a| {
            other
                .reads
                .iter()
                .chain(&other.writes)
                .any(|b| overlap(a, b))
        }) || other
            .writes
            .iter()
            .any(|a| self.reads.iter().any(|b| overlap(a, b)))
    }

    pub(in crate::agent) fn permits(&self, paths: &[PathBuf], unbounded: bool) -> bool {
        !unbounded
            && !paths.is_empty()
            && paths
                .iter()
                .all(|path| self.reads.contains(path) || self.writes.contains(path))
    }
}

/// Resolve literal local regular files through the native workspace boundary. Reject directory
/// creation, links, aliases, selectors, and incomplete footprints instead of assuming independence.
fn resource(workspace: &Workspace, literal: &str, writing: bool) -> Option<PathBuf> {
    let path = Path::new(literal);
    if literal.contains([':', '?', '@'])
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return None;
    }
    if path.components().any(|part| matches!(part, std::path::Component::Normal(name) if name.to_string_lossy().starts_with('.'))) {
        return None;
    }
    let lexical = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.root().join(path)
    };
    let resolved = workspace.resolve(literal).ok()?;
    if lexical != resolved || !resolved.parent()?.is_dir() {
        return None;
    }
    match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return None;
                }
            }
            // Without a portable hard-link count, existing files remain serial on Windows.
            #[cfg(not(unix))]
            {
                return None;
            }
        }
        Err(error) if writing && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    Some(resolved)
}

pub(in crate::agent) fn footprint(
    workspace: &Workspace,
    mode: crate::config::types::EditMode,
    call: &ToolCall,
) -> Option<Footprint> {
    let (targets, writing) = match call.name.as_str() {
        "read" if call.arguments.get("paths").is_none() => {
            (vec![call.arguments["path"].as_str()?.to_owned()], false)
        }
        "write" => (vec![call.arguments["path"].as_str()?.to_owned()], true),
        // The native parser is the authority for every source, destination and rename target.
        "edit" => (tools::editing_targets(mode, &call.arguments).ok()?, true),
        _ => return None,
    };
    if targets.is_empty() {
        return None;
    }
    let paths = targets
        .iter()
        .map(|path| resource(workspace, path, writing))
        .collect::<Option<Vec<_>>>()?;
    Some(Footprint {
        reads: paths.clone(),
        writes: if writing { paths } else { Vec::new() },
    })
}

impl Agent<'_> {
    /// A maximal independent prefix, bounded by the controller's configured concurrency. A
    /// conflicting resource or unknown effect closes the wave; nothing jumps a serial barrier.
    pub(in crate::agent) fn dependency_wave(&mut self, calls: &[ToolCall]) -> Vec<ToolCall> {
        let mut admitted = Vec::new();
        let mut footprints = Vec::new();
        for call in calls.iter().take(self.config.task.max_concurrency.min(32)) {
            if ["pre_tool_use", "post_tool_use"].iter().any(|event| {
                crate::agent::safety::hooks::wanted_for_tool(
                    self.tool_hooks(),
                    self.hook_plugins().as_deref(),
                    event,
                    &call.name,
                )
            }) || self.repro_first_fires(call, crate::config::types::ReproFirst::Gate)
                || self.speculative_pending(call)
            {
                break;
            }
            let mut effective = call.clone();
            if crate::agent::window::file_handles::expand(&self.workspace, &mut effective).is_err()
            {
                break;
            }
            let Some(resources) = footprint(&self.workspace, self.edit_mode(), &effective) else {
                break;
            };
            if footprints
                .iter()
                .any(|previous| resources.conflicts(previous))
            {
                break;
            }
            footprints.push(resources);
            admitted.push(effective);
        }
        admitted
    }

    /// Already schema-admitted, dependency-independent native calls. This shares the ordinary
    /// native worker pump, including live mutation/repair/confirmation callbacks and checkpoints.
    pub(in crate::agent) fn dispatch_dependency_wave(
        &mut self,
        backend: &mut Backend,
        calls: &[ToolCall],
        ui: &mut dyn Ui,
    ) -> Vec<(ToolOutput, f64)> {
        let blocked = |reason: &str| {
            ToolOutput::err(reason)
                .with_status(tools::ToolStatus::Blocked)
                .with_effects(tools::surface::outcome::Effects::None)
        };
        let mut results = vec![None; calls.len()];
        let mut ready = Vec::new();
        let mut positions = Vec::new();
        for (index, call) in calls.iter().enumerate() {
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
            } else {
                positions.push(index);
                ready.push(call.clone());
            }
        }
        if !ready.is_empty() {
            let outputs = self
                .run_background_tools(&ready, ui, Some(backend))
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
        let results: Vec<_> = results
            .into_iter()
            .map(|result| result.expect("one settled outcome per admitted call"))
            .collect();
        for (call, (output, _)) in calls.iter().zip(&results) {
            crate::agent::modes::lifecycle::Lifecycle::dispatched(self, call, output);
        }
        results
    }
}

#[cfg(test)]
pub(super) mod tests;

//! The native execution boundary shared by foreground, worker and parallel controllers.
//! Admission, hooks and role-owned capabilities stay with the controller. Once admitted, native
//! dispatch, pre-start cancellation, panic containment and effect certainty have one implementation.
use crate::tools::{
    ToolOutput, ToolStatus,
    integrations::ext::{Registry, ToolContext, ToolEvent},
    surface::outcome::Effects,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::RefCell, path::PathBuf};

thread_local! { static MUTATION: RefCell<Option<crate::runtime::cancellation::MutationCancellation>> = const { RefCell::new(None) }; }

pub fn mutation_scope<R>(
    scope: &crate::runtime::cancellation::MutationCancellation,
    action: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<crate::runtime::cancellation::MutationCancellation>);
    impl Drop for Restore {
        fn drop(&mut self) {
            MUTATION.with(|scope| {
                *scope.borrow_mut() = self.0.take();
            });
        }
    }
    let prior = MUTATION.with(|current| current.replace(Some(scope.clone())));
    let _restore = Restore(prior);
    action()
}

pub(crate) fn before_commit() -> Result<(), String> {
    MUTATION.with(|scope| {
        if scope
            .borrow()
            .as_ref()
            .is_some_and(|scope| scope.commit.is_cancelled())
        {
            Err("session terminated before document commit".into())
        } else {
            Ok(())
        }
    })
}

pub(crate) fn committed(paths: impl IntoIterator<Item = PathBuf>) {
    crate::runtime::execution::committed(paths);
}
pub(crate) fn unsettled() {
    crate::runtime::execution::unsettled();
}
use crate::runtime::execution::Capture;

pub fn before_start() -> ToolOutput {
    ToolOutput::err("interrupted by the user before this ran")
        .with_status(ToolStatus::Aborted)
        .with_effects(Effects::None)
}

/// Hands a foreground job to the managed `bg` owner, which keeps it running after its controller
/// returns.
pub fn adopt_into(
    registry: &Registry,
    job: crate::tools::exec::background::AdoptedJob,
) -> Result<String, String> {
    registry
        .get("bg")
        .ok_or_else(|| "managed background tool is unavailable".to_string())?
        .adopt_job(job)
}

pub fn with_dispatch(
    registry: &Registry,
    tool: &dyn crate::tools::integrations::ext::Tool,
    context: &ToolContext<'_>,
    args: &Value,
    emit: &(dyn Fn(ToolEvent) + Sync),
    cancel: &AtomicBool,
    dispatch: &mut dyn FnMut(crate::tools::integrations::ext::ToolCallback) -> ToolOutput,
) -> ToolOutput {
    run(cancel, || {
        let mut uncertain = false;
        let mut changes = Vec::new();
        let mut output = tool.call_with_dispatch(
            context,
            args,
            emit,
            cancel,
            &|job| adopt_into(registry, job),
            &mut |request| {
                if uncertain {
                    return ToolOutput::err("callback refused after uncertain effects")
                        .with_status(ToolStatus::Skipped)
                        .with_effects(Effects::None);
                }
                let output = dispatch(request);
                changes.extend(output.execution.changed_paths.iter().cloned());
                uncertain |= crate::tools::surface::outcome::effects(&output) == Effects::Unknown;
                output
            },
        );
        output.execution.changed_paths.extend(changes);
        output.execution.changed_paths.sort();
        output.execution.changed_paths.dedup();
        if uncertain && crate::tools::surface::outcome::effects(&output) != Effects::Unknown {
            output.with_effects(Effects::Unknown)
        } else {
            output
        }
    })
}

pub fn run(cancel: &AtomicBool, action: impl FnOnce() -> ToolOutput) -> ToolOutput {
    contained(cancel, Effects::Unknown, action)
}

fn contained(
    cancel: &AtomicBool,
    panic_effects: Effects,
    action: impl FnOnce() -> ToolOutput,
) -> ToolOutput {
    if cancel.load(Ordering::SeqCst) {
        return before_start();
    }
    let capture = Capture::begin();
    let mut panicked = false;
    let mut output =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)).unwrap_or_else(|_| {
            panicked = true;
            ToolOutput::err("tool worker panicked").with_effects(panic_effects)
        });
    // Timeouts come only from owners that kill and reap the process group first, so they are
    // ordinary known failures. An abort, or an error raised under cancellation, stays unsettled.
    if output.execution.effects == Effects::Unknown
        || (output.execution.effects == Effects::Known
            && (ToolStatus::from_output(&output) == ToolStatus::Aborted
                || (output.is_error && cancel.load(Ordering::SeqCst))))
    {
        unsettled();
    }
    let facts = capture.finish(!output.is_error && !cancel.load(Ordering::SeqCst));
    if let Some(status) = facts
        .status
        .filter(|status| matches!(status, ToolStatus::Aborted | ToolStatus::Timeout))
        && (status == ToolStatus::Aborted || output.execution.status != Some(ToolStatus::Aborted))
    {
        output = output.with_status(status);
        output.is_error = true;
    }
    output.execution.changed_paths.extend(facts.changed_paths);
    output.execution.changed_paths.sort();
    output.execution.changed_paths.dedup();
    // A panic after a recorded commit is uncertain even for a call admitted as a read.
    let committed_then_panicked = panicked && !output.execution.changed_paths.is_empty();
    if committed_then_panicked && facts.effects != Effects::Unknown {
        unsettled();
    }
    if facts.effects == Effects::Unknown || committed_then_panicked {
        output.with_effects(Effects::Unknown)
    } else {
        output
    }
}

/// Ordinary tools only. Controller-owned tools (memory, task, checkpoints, approvals) cannot be
/// reached through this function and must re-enter their owning controller for callbacks.
pub fn native(
    context: &ToolContext<'_>,
    registry: &Registry,
    name: &str,
    args: &Value,
    emit: &(dyn Fn(ToolEvent) + Sync),
    cancel: &AtomicBool,
) -> Option<ToolOutput> {
    native_with(
        Effects::Unknown,
        context,
        registry,
        name,
        args,
        emit,
        cancel,
    )
}

/// [`native`] for a call the controller admitted as a parallel read. A panic is a known failure
/// with no effects, except for shell commands: a process may have started and outlived it.
pub fn native_read(
    context: &ToolContext<'_>,
    registry: &Registry,
    name: &str,
    args: &Value,
    emit: &(dyn Fn(ToolEvent) + Sync),
    cancel: &AtomicBool,
) -> Option<ToolOutput> {
    let panic_effects = if name == "bash" {
        Effects::Unknown
    } else {
        Effects::None
    };
    native_with(panic_effects, context, registry, name, args, emit, cancel)
}

fn native_with(
    panic_effects: Effects,
    context: &ToolContext<'_>,
    registry: &Registry,
    name: &str,
    args: &Value,
    emit: &(dyn Fn(ToolEvent) + Sync),
    cancel: &AtomicBool,
) -> Option<ToolOutput> {
    if !crate::tools::BUILTIN.contains(&name) && name != "artifact_read" && !registry.contains(name)
    {
        return None;
    }
    Some(contained(cancel, panic_effects, || match name {
        "bash" => {
            let stream = |bytes: &[u8]| {
                emit(ToolEvent {
                    kind: "output".into(),
                    name: name.into(),
                    text: String::from_utf8_lossy(bytes).into_owned(),
                    artifact: None,
                })
            };
            let adopt = |job| {
                adopt_into(
                    registry,
                    crate::tools::exec::background::AdoptedJob::Shell(Box::new(job)),
                )
            };
            super::shell::bash_tool_streaming_with_adopter(
                context.workspace,
                args,
                context.config.shell.bash_timeout_secs,
                context.config.shell.auto_background_secs,
                &stream,
                cancel,
                &adopt,
            )
            .unwrap_or_else(ToolOutput::err)
        }
        "read" => crate::tools::read_tool_streaming(
            context.workspace,
            args,
            cancel,
            crate::tools::model_output_limit(context.config.context_window),
        ),
        "artifact_read" => context
            .workspace
            .artifacts()
            .map(|store| store.call(args))
            .unwrap_or_else(|| ToolOutput::err("artifacts unavailable")),
        name if crate::tools::BUILTIN.contains(&name) => {
            crate::tools::execute(context.workspace, name, args, 120)
        }
        name => registry
            .get(name)
            .expect("registered tool")
            .call_streaming(context, args, emit, cancel),
    }))
}

#[cfg(all(test, unix))]
mod process_outcome_tests;

#[cfg(test)]
mod tests;

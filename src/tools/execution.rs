//! The native execution boundary shared by foreground, worker and parallel controllers.
//! Admission, hooks and role-owned capabilities stay with the controller. Once admitted, native
//! dispatch, pre-start cancellation, panic containment and effect certainty have one implementation.
use super::{
    ToolOutput, ToolStatus,
    ext::{Registry, ToolContext, ToolEvent},
    outcome::Effects,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{cell::RefCell, path::PathBuf};

thread_local! { static MUTATION: RefCell<Option<crate::runtime::cancellation::MutationCancellation>> = const { RefCell::new(None) }; }

pub(crate) fn mutation_scope<R>(
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

pub(crate) fn before_start() -> ToolOutput {
    ToolOutput::err("interrupted by the user before this ran")
        .with_status(ToolStatus::Aborted)
        .with_effects(Effects::None)
}

pub(crate) fn with_dispatch(
    tool: &dyn super::ext::Tool,
    context: &ToolContext<'_>,
    args: &Value,
    emit: &(dyn Fn(ToolEvent) + Sync),
    cancel: &AtomicBool,
    dispatch: &mut dyn FnMut(super::ext::ToolCallback) -> ToolOutput,
) -> ToolOutput {
    run(cancel, || {
        let mut uncertain = false;
        let mut changes = Vec::new();
        let mut output = tool.call_with_dispatch(context, args, emit, cancel, &mut |request| {
            if uncertain {
                return ToolOutput::err("callback refused after uncertain effects")
                    .with_status(ToolStatus::Skipped)
                    .with_effects(Effects::None);
            }
            let output = dispatch(request);
            changes.extend(output.execution.changed_paths.iter().cloned());
            uncertain |= super::outcome::effects(&output) == Effects::Unknown;
            output
        });
        output.execution.changed_paths.extend(changes);
        output.execution.changed_paths.sort();
        output.execution.changed_paths.dedup();
        if uncertain && super::outcome::effects(&output) != Effects::Unknown {
            output.with_effects(Effects::Unknown)
        } else {
            output
        }
    })
}

pub(crate) fn run(cancel: &AtomicBool, action: impl FnOnce() -> ToolOutput) -> ToolOutput {
    if cancel.load(Ordering::SeqCst) {
        return before_start();
    }
    let capture = Capture::begin();
    let mut output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
        .unwrap_or_else(|_| ToolOutput::err("tool worker panicked").with_effects(Effects::Unknown));
    if output.execution.effects == Effects::Unknown
        || (output.execution.effects == Effects::Known
            && (matches!(
                ToolStatus::from_output(&output),
                ToolStatus::Timeout | ToolStatus::Aborted
            ) || (output.is_error && cancel.load(Ordering::SeqCst))))
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
    if facts.effects == Effects::Unknown {
        output.with_effects(Effects::Unknown)
    } else {
        output
    }
}

/// Ordinary tools only. Controller-owned tools (memory, task, checkpoints, approvals) cannot be
/// reached through this function and must re-enter their owning controller for callbacks.
pub(crate) fn native(
    context: &ToolContext<'_>,
    registry: &Registry,
    name: &str,
    args: &Value,
    emit: &(dyn Fn(ToolEvent) + Sync),
    cancel: &AtomicBool,
) -> Option<ToolOutput> {
    if !super::BUILTIN.contains(&name) && name != "artifact_read" && !registry.contains(name) {
        return None;
    }
    Some(run(cancel, || match name {
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
                registry
                    .get("bg")
                    .ok_or_else(|| "managed background tool is unavailable".to_string())?
                    .adopt_foreground(job)
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
        "read" => super::read_tool_streaming(context.workspace, args, cancel),
        "artifact_read" => context
            .workspace
            .artifacts()
            .map(|store| store.call(args))
            .unwrap_or_else(|| ToolOutput::err("artifacts unavailable")),
        name if super::BUILTIN.contains(&name) => {
            super::execute(context.workspace, name, args, 120)
        }
        name => registry
            .get(name)
            .expect("registered tool")
            .call_streaming(context, args, emit, cancel),
    }))
}

#[cfg(all(test, unix))]
mod process_outcome_tests {
    use super::*;
    #[test]
    fn nested_catches_cannot_erase_interrupted_process_effects_or_termination() {
        let output = run(&AtomicBool::new(false), || {
            let _ = run(&AtomicBool::new(false), || {
                let mut command = std::process::Command::new("/bin/sh");
                command.args(["-c", "sleep 5"]);
                let _ = crate::runtime::process::run(
                    command,
                    None,
                    std::time::Duration::from_millis(20),
                    128,
                    &AtomicBool::new(false),
                    &mut |_, _| {},
                );
                ToolOutput::ok("caller caught the physical timeout".into())
            });
            ToolOutput::ok("outer caller also returned success".into())
        });
        assert_eq!(output.execution.status, Some(ToolStatus::Timeout));
        assert_eq!(output.execution.effects, Effects::Unknown);
        assert!(output.is_error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::outcome;
    #[test]
    fn an_unsettled_mutation_survives_nested_error_catching() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial");
        let cancel = AtomicBool::new(false);
        let output = run(&cancel, || {
            let _caught = run(&cancel, || {
                std::fs::write(&path, "effect remains").unwrap();
                committed([path.clone()]);
                unsettled();
                ToolOutput::err("recovery could not complete")
            });
            ToolOutput::ok("caller caught the error".into())
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "effect remains");
        assert_eq!(output.execution.changed_paths, [path]);
        assert_eq!(output.execution.effects, Effects::Unknown);
        assert_eq!(
            outcome::result_phase("write", &Value::Null, &output),
            crate::runtime::Phase::Incomplete
        );
    }
    #[test]
    fn catching_a_nested_panic_cannot_publish_known_effects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested");
        let cancel = AtomicBool::new(false);
        let output = run(&cancel, || {
            let _caught = run(&cancel, || {
                std::fs::write(&path, "effect before panic").unwrap();
                committed([path.clone()]);
                panic!("nested tool panicked");
            });
            ToolOutput::ok("handled the nested error".into())
        });
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "effect before panic"
        );
        assert_eq!(output.execution.changed_paths, [path]);
        assert_eq!(output.execution.effects, Effects::Unknown);
    }
    #[test]
    fn panic_after_effect_preserves_change_evidence_and_uncertainty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changed");
        let output = run(&AtomicBool::new(false), || {
            std::fs::write(&path, "completed effect").unwrap();
            committed([path.clone()]);
            panic!("failure after effect");
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "completed effect");
        assert_eq!(output.execution.changed_paths, [path]);
        assert_eq!(outcome::effects(&output), Effects::Unknown);
        assert_eq!(
            outcome::result_phase("write", &Value::Null, &output),
            crate::runtime::Phase::Incomplete
        );
    }
    #[test]
    fn callback_uncertainty_survives_a_tool_catching_errors() {
        struct Catching;
        impl super::super::ext::Tool for Catching {
            fn needs_approval(&self, _: &Value) -> bool {
                false
            }
            fn name(&self) -> &str {
                "catching"
            }
            fn schema(&self) -> Value {
                Value::Null
            }
            fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
                unreachable!()
            }
            fn call_with_dispatch(
                &self,
                _: &ToolContext,
                _: &Value,
                _: &(dyn Fn(ToolEvent) + Sync),
                _: &AtomicBool,
                dispatch: &mut dyn FnMut(super::super::ext::ToolCallback) -> ToolOutput,
            ) -> ToolOutput {
                for _ in 0..2 {
                    dispatch(super::super::ext::ToolCallback::Call {
                        name: "write".into(),
                        arguments: Value::Null,
                    });
                }
                ToolOutput::structured(serde_json::json!({"ignored_errors":true}))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let workspace = super::super::Workspace::new(dir.path()).unwrap();
        let config = crate::config::types::Config::default();
        let context = ToolContext {
            workspace: &workspace,
            config: &config,
            mutation_authorizer: None,
            confirm: None,
        };
        let mut count = 0;
        let output = with_dispatch(
            &Catching,
            &context,
            &Value::Null,
            &|_| {},
            &AtomicBool::new(false),
            &mut |_| {
                count += 1;
                ToolOutput::err("connection lost after write").with_effects(Effects::Unknown)
            },
        );
        assert_eq!(count, 1);
        assert_eq!(outcome::effects(&output), Effects::Unknown);
        assert!(output.is_error);
    }
    #[test]
    fn skipped_and_uncertain_effects_are_distinct() {
        let cancel = AtomicBool::new(true);
        let output = run(&cancel, || panic!("must not run"));
        assert_eq!(outcome::effects(&output), Effects::None);
        cancel.store(false, Ordering::SeqCst);
        let output = run(&cancel, || panic!("effect may have happened"));
        assert_eq!(outcome::effects(&output), Effects::Unknown);
        assert_eq!(
            outcome::result_phase("write", &Value::Null, &output),
            crate::runtime::Phase::Incomplete
        );
        let output = run(&cancel, || {
            ToolOutput::err("interrupted").with_status(ToolStatus::Aborted)
        });
        assert_eq!(outcome::effects(&output), Effects::Unknown);
    }
}

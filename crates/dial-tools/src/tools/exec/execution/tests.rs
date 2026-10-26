use super::*;
use crate::tools::surface::outcome;
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
    impl crate::tools::integrations::ext::Tool for Catching {
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
            dispatch: &mut dyn FnMut(crate::tools::integrations::ext::ToolCallback) -> ToolOutput,
        ) -> ToolOutput {
            for _ in 0..2 {
                dispatch(crate::tools::integrations::ext::ToolCallback::Call {
                    name: "write".into(),
                    arguments: Value::Null,
                });
            }
            ToolOutput::structured(serde_json::json!({"ignored_errors":true}))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
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
fn a_panicking_read_is_a_known_failure_unless_it_recorded_a_commit() {
    let cancel = AtomicBool::new(false);
    let output = contained(&cancel, Effects::None, || panic!("read worker bug"));
    assert!(output.is_error);
    assert_eq!(outcome::effects(&output), Effects::None);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unexpected");
    let output = contained(&cancel, Effects::None, || {
        std::fs::write(&path, "x").unwrap();
        committed([path.clone()]);
        panic!("a read that wrote");
    });
    assert_eq!(
        outcome::effects(&output),
        Effects::Unknown,
        "a recorded commit keeps the panic uncertain"
    );
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

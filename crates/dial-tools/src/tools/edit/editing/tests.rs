use super::*;
use crate::runtime::execution::Effects;
use crate::tools::{ToolOutput, WriteObserver, execute};
use std::fs;
use std::sync::{Mutex, atomic::AtomicBool};

#[derive(Default)]
struct Observed(Mutex<Vec<PathBuf>>);
impl WriteObserver for Observed {
    fn before_write(&self, path: &Path) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(path.to_owned());
        Ok(())
    }
}

fn workspace(text: &str) -> (tempfile::TempDir, Workspace, PathBuf, Arc<Observed>) {
    let (dir, ws) = crate::tools::testkit::workspace();
    let observed = Arc::new(Observed::default());
    let ws = ws
        .with_edit_mode(EditMode::Replace)
        .with_observer(observed.clone());
    let path = ws.root().join("a.txt");
    fs::write(&path, text).unwrap();
    (dir, ws, path, observed)
}

fn replace(ws: &Workspace, old: &str, new: &str) -> ToolOutput {
    execute(
        ws,
        "edit",
        &json!({"path":"a.txt", "old_string":old, "new_string":new}),
        20,
    )
}

fn assert_observational(output: &ToolOutput, outcome: &str) {
    assert_eq!(
        output.data.as_ref().unwrap()["edit"]["outcome"],
        outcome,
        "{}",
        output.text
    );
    assert_eq!(output.execution.effects, Effects::None, "{}", output.text);
    assert!(output.execution.changed_paths.is_empty(), "{}", output.text);
}

#[test]
fn whole_write_refuses_identical_bytes_without_committing_but_allows_changed_overwrites() {
    for mode in [EditMode::Replace, EditMode::Hashline] {
        for before in ["before_value\n", "before_value\r\n", ""] {
            let (_dir, ws, path, observed) = workspace(before);
            let ws = ws.with_edit_mode(mode);
            ws.store().record_all(&path, &hashline::normalize(before));
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            let write = |content: &str| {
                crate::tools::exec::execution::run(&AtomicBool::new(false), || {
                    execute(
                        &ws,
                        "write",
                        &json!({"path":"a.txt", "content":content}),
                        20,
                    )
                })
            };
            let unchanged = write(before);
            assert!(unchanged.is_error, "mode={mode:?}: {}", unchanged.text);
            assert!(
                unchanged.text.contains("would not change"),
                "{}",
                unchanged.text
            );
            assert!(unchanged.execution.changed_paths.is_empty());
            assert!(observed.0.lock().unwrap().is_empty());
            assert_eq!(fs::read_to_string(&path).unwrap(), before);
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);

            let changed = write("desired_value\n");
            assert!(!changed.is_error, "mode={mode:?}: {}", changed.text);
            assert!(changed.text.contains("overwrote"), "{}", changed.text);
            assert_eq!(observed.0.lock().unwrap().len(), 1);
            assert_eq!(
                changed.execution.changed_paths.as_slice(),
                std::slice::from_ref(&path)
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "desired_value\n");
        }
    }
}

#[test]
fn witnessed_exact_retry_and_identity_are_stable_observations_not_commits() {
    let (_dir, ws, path, observed) = workspace("before_value\n");
    ws.store().record_all(&path, "before_value\n");
    let first = replace(&ws, "before_value", "desired_value");
    assert!(!first.is_error, "{}", first.text);
    assert_eq!(first.data.as_ref().unwrap()["edit"]["outcome"], "applied");
    assert_eq!(observed.0.lock().unwrap().len(), 1);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    for old in [
        "before_value",
        "desired_value",
        "before_value",
        "desired_value",
    ] {
        let output = replace(&ws, old, "desired_value");
        assert!(!output.is_error, "{}", output.text);
        assert_observational(&output, "already_applied");
    }
    assert_eq!(observed.0.lock().unwrap().len(), 1);
    assert_eq!(fs::read_to_string(&path).unwrap(), "desired_value\n");
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
}

#[test]
fn desired_text_without_unique_current_and_prior_witnesses_is_not_retry_proof() {
    let cases = [
        ("desired\n", None, false, "desired", "desired"),
        ("desired\n", Some("previous\n"), false, "desired", "desired"),
        ("desired desired\n", None, true, "desired", "desired"),
        ("desired\n", None, true, "missing", "desired"),
        (
            "desired\nother_changed\n",
            Some("old\nother\n"),
            true,
            "old",
            "desired",
        ),
        ("\n", Some("old\n"), true, "old", ""),
    ];
    for (current, prior, read_current, old, new) in cases {
        let (_dir, ws, path, observed) = workspace(current);
        if let Some(prior) = prior {
            ws.store().record_all(&path, prior);
        }
        if read_current {
            ws.store().record_all(&path, current);
        }
        let output = replace(&ws, old, new);
        assert!(output.is_error, "old={old:?} new={new:?}: {}", output.text);
        assert_eq!(output.execution.effects, Effects::None);
        assert!(output.execution.changed_paths.is_empty());
        assert!(observed.0.lock().unwrap().is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), current);
    }
}

#[test]
fn satisfied_preparation_rechecks_bytes_and_cancellation_before_reporting_success() {
    let (_dir, ws, path, observed) = workspace("desired\n");
    ws.store().record_all(&path, "desired\n");
    let args = json!({"path":"a.txt", "old_string":"desired", "new_string":"desired"});
    let prepared = prepare(&ws, &args).unwrap();
    fs::write(&path, "external\n").unwrap();
    let error = finish_prepared(&ws, prepared, true, &Execution::default()).unwrap_err();
    assert!(matches!(error, PrepareError::SourceChanged(_)));
    let output = prepare_error_output(error).unwrap();
    assert!(output.is_error);
    assert_observational(&output, "source_changed");
    assert_eq!(fs::read_to_string(&path).unwrap(), "external\n");

    ws.store().record_all(&path, "external\n");
    let prepared = prepare(
        &ws,
        &json!({"path":"a.txt", "old_string":"external", "new_string":"external"}),
    )
    .unwrap();
    let cancelled = AtomicBool::new(true);
    let execution = Execution {
        cancel: Some(&cancelled),
        ..Default::default()
    };
    assert!(finish_prepared(&ws, prepared, true, &execution).is_err());
    assert!(observed.0.lock().unwrap().is_empty());
}

#[test]
fn missing_target_has_a_machine_readable_failure_without_crediting_new_text() {
    let (_dir, ws, path, _) = workspace("desired\n");
    ws.store().record_all(&path, "desired\n");
    let output = replace(&ws, "absent", "desired");
    assert!(output.is_error, "{}", output.text);
    assert_observational(&output, "target_not_found");
}

#[test]
fn notebook_identity_requires_the_raw_metadata_revision() {
    let (dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.with_edit_mode(EditMode::Replace);
    let path = ws.root().join("a.ipynb");
    let mut notebook = json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"stable-code-cell","cell_type":"code","metadata":{},"source":["value = 1\n"],"outputs":[],"execution_count":null}]});
    fs::write(&path, notebook.to_string()).unwrap();
    let read = execute(&ws, "read", &json!({"path":"a.ipynb"}), 20);
    assert!(!read.is_error, "{}", read.text);
    let args = json!({"path":"a.ipynb", "old_string":"value = 1", "new_string":"value = 1"});
    let output = execute(&ws, "edit", &args, 20);
    assert!(!output.is_error, "{}", output.text);
    assert_observational(&output, "already_applied");
    notebook["metadata"] = json!({"external":true});
    let external = notebook.to_string();
    fs::write(&path, &external).unwrap();
    let refused = execute(&ws, "edit", &args, 20);
    assert!(refused.is_error, "{}", refused.text);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.ipynb")).unwrap(),
        external
    );
}

#[test]
fn replace_all_unchanged_fuzzy_matches_still_require_witnesses() {
    use super::super::edit_native::{
        error::EditError,
        fuzzy::{ReplaceOutcome, replace_text},
    };
    for (current, old, new) in [
        ("same\n", "same", "same"),
        (
            "    first\n    second\n",
            "  first\n  second",
            "    first\n    second",
        ),
    ] {
        let calls = std::cell::Cell::new(0);
        let refuse = |_, _| {
            calls.set(calls.get() + 1);
            Err(EditError::apply("no source witness"))
        };
        assert!(replace_text(current, old, new, true, true, None, &refuse).is_err());
        assert!(calls.get() > 0);
        let result = replace_text(current, old, new, true, true, None, &|_, _| Ok(())).unwrap();
        assert!(matches!(result, ReplaceOutcome::NoChange), "{result:?}");
    }
}

#[test]
fn hashline_exact_noop_is_stable_but_stale_and_mixed_batches_remain_atomic_refusals() {
    let (_dir, ws, path, observed) = workspace("desired\n");
    let ws = ws.with_edit_mode(EditMode::Hashline);
    let tag = ws.store().record_all(&path, "desired\n");
    let args = json!({"input":format!("[a.txt#{tag}]\nPUT 1.=1:\n+desired\n")});
    for _ in 0..4 {
        let output = execute(&ws, "edit", &args, 20);
        assert!(!output.is_error, "{}", output.text);
        assert_observational(&output, "already_applied");
    }
    let other = ws.root().join("b.txt");
    fs::write(&other, "before\n").unwrap();
    let other_tag = ws.store().record_all(&other, "before\n");
    let mixed = execute(
        &ws,
        "edit",
        &json!({"input":format!("[a.txt#{tag}]\nPUT 1.=1:\n+desired\n[b.txt#{other_tag}]\nPUT 1.=1:\n+after\n")}),
        20,
    );
    assert!(mixed.is_error, "{}", mixed.text);
    assert!(mixed.execution.changed_paths.is_empty());
    assert_eq!(fs::read_to_string(&other).unwrap(), "before\n");
    fs::write(&path, "external\n").unwrap();
    let stale = execute(&ws, "edit", &args, 20);
    assert!(stale.is_error, "{}", stale.text);
    assert_eq!(fs::read_to_string(&path).unwrap(), "external\n");
    assert!(observed.0.lock().unwrap().is_empty());
}

#[test]
fn parse_repair_that_reverts_the_request_is_not_already_applied() {
    let (_dir, mut ws, _, observed) = workspace("");
    ws.edit_settings.auto_repair.enabled = true;
    let path = ws.root().join("a.rs");
    let original = "fn main() {}\n";
    fs::write(&path, original).unwrap();
    ws.store().record_all(&path, original);
    let repair = |_: &EditRepairRequest| Ok(original.to_owned());
    let execution = Execution {
        repair: Some(&repair),
        ..Default::default()
    };
    let args = json!({"path":"a.rs", "old_string":"fn main() {}", "new_string":"fn main( {"});
    let prepared = prepare(&ws, &args).unwrap();
    let error = finish_prepared(&ws, prepared, true, &execution).unwrap_err();
    assert!(matches!(&error, PrepareError::Refused(_)), "{error}");
    assert!(
        error.to_string().contains("no change after parse repair"),
        "{error}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
    assert!(observed.0.lock().unwrap().is_empty());
}

#[test]
fn optional_check_is_boolean_and_valid_in_every_edit_grammar() {
    for mode in [
        EditMode::Hashline,
        EditMode::Replace,
        EditMode::Patch,
        EditMode::ApplyPatch,
        EditMode::Sloppy,
    ] {
        for check in [json!(true), json!(false), Value::Null] {
            assert!(
                validate_shape(mode, &json!({"check":check})).is_ok(),
                "{mode:?}"
            );
        }
        for check in [json!("yes"), json!({}), json!(1)] {
            assert!(
                validate_shape(mode, &json!({"check":check})).is_err(),
                "{mode:?}"
            );
        }
    }
}

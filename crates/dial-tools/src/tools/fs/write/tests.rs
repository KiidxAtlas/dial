use super::*;
use crate::tools::testkit::*;
use std::fs;

#[test]
fn batch_creates_files_with_ordered_structured_receipts() {
    let (dir, ws) = workspace();
    let out = run(
        &ws,
        "write",
        json!({"files":[{"path":"b.txt","content":"abc"},{"path":"nested/a.txt","content":""}],"verify":["a.test"]}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(fs::read(dir.path().join("b.txt")).unwrap(), b"abc");
    assert_eq!(fs::read(dir.path().join("nested/a.txt")).unwrap(), b"");
    let data = out.data.unwrap();
    assert_eq!(data["files"][0]["path"], "b.txt");
    assert_eq!(
        data["files"][0]["sha256"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(data["files"][1]["bytes"], 0);
    assert_eq!(data["verification"]["status"], "requested");
}

#[test]
fn malformed_batch_and_failed_preconditions_leave_every_destination_untouched() {
    let (dir, ws) = workspace();
    fs::write(dir.path().join("existing"), "old").unwrap();
    for entry in [
        json!({"path":"existing","content":"new","if_absent":true}),
        json!({"path":"existing","content":"new","expected_sha256":"0".repeat(64)}),
        json!({"path":"missing","content":"new","expected_sha256":"0".repeat(64)}),
        json!({"path":"first","content":"duplicate"}),
        json!({"path":"first/child","content":"overlap"}),
        json!({"path":"bad","content":"x","encoding":"base64"}),
    ] {
        let out = run(
            &ws,
            "write",
            json!({"files":[{"path":"first","content":"abc"},entry]}),
        );
        assert!(out.is_error, "{}", out.text);
        assert!(!dir.path().join("first").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("existing")).unwrap(),
            "old"
        );
    }
}

#[test]
fn opt_in_idempotency_preserves_legacy_no_change_errors_and_read_guard() {
    let (dir, ws) = workspace();
    let args = json!({"path":"a","content":"abc","idempotent":true,"receipt":true});
    let created = run(&ws, "write", args.clone());
    assert!(!created.is_error, "{}", created.text);
    let modified = fs::metadata(dir.path().join("a"))
        .unwrap()
        .modified()
        .unwrap();
    let repeated = run(&ws, "write", args);
    assert!(!repeated.is_error, "{}", repeated.text);
    assert_eq!(
        repeated.data.as_ref().unwrap()["files"][0]["outcome"],
        "unchanged"
    );
    assert_eq!(repeated.data.as_ref().unwrap()["changed"], false);
    assert!(repeated.execution.changed_paths.is_empty());
    assert_eq!(
        repeated.execution.effects,
        crate::runtime::execution::Effects::None
    );
    assert_eq!(
        repeated.data.as_ref().unwrap()["edit"]["outcome"],
        "already_applied"
    );
    assert_eq!(
        fs::metadata(dir.path().join("a"))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    assert!(run(&ws, "write", json!({"path":"a","content":"abc"})).is_error);
    assert!(
        run(
            &ws,
            "write",
            json!({"path":"a","content":"abc","idempotent":true,"if_absent":true})
        )
        .is_error
    );
    fs::write(dir.path().join("unread"), "abc").unwrap();
    assert!(
        run(
            &ws,
            "write",
            json!({"path":"unread","content":"abc","idempotent":true})
        )
        .is_error
    );
}

#[test]
fn artifact_source_is_exact_and_hash_checked() {
    let (dir, ws) = workspace();
    let store =
        crate::storage::artifact::ArtifactStore::new(&dir.path().join("artifacts")).unwrap();
    let content = b"[literal#ABCD]\n1:not a read projection\n";
    let uri = store.retain_exact_bytes(content).unwrap();
    let hash = uri.strip_prefix("artifact://").unwrap();
    let ws = ws.with_artifacts(store);
    let out = run(
        &ws,
        "write",
        json!({"path":"copy.txt","source":{"artifact":uri,"sha256":hash}}),
    );
    assert!(!out.is_error, "{}", out.text);
    assert_eq!(fs::read(dir.path().join("copy.txt")).unwrap(), content);
    let out = run(
        &ws,
        "write",
        json!({"path":"bad.txt","source":{"artifact":uri,"sha256":"0".repeat(64)}}),
    );
    assert!(out.is_error, "{}", out.text);
    assert!(!dir.path().join("bad.txt").exists());
}

#[test]
fn expected_revision_and_mixed_idempotent_batch() {
    let (dir, ws) = workspace();
    let out = run(&ws, "write", json!({"path":"a","content":"abc"}));
    assert!(!out.is_error, "{}", out.text);
    let hash = out.data.unwrap()["files"][0]["sha256"].clone();
    let out = run(
        &ws,
        "write",
        json!({"path":"a","content":"new","expected_sha256":hash}),
    );
    assert!(!out.is_error, "{}", out.text);
    let out = run(
        &ws,
        "write",
        json!({"files":[{"path":"a","content":"new","idempotent":true},{"path":"b","content":"second"}]}),
    );
    assert!(!out.is_error, "{}", out.text);
    let data = out.data.unwrap();
    assert_eq!(data["files"][0]["outcome"], "unchanged");
    assert_eq!(data["files"][1]["outcome"], "created");
    assert_eq!(fs::read_to_string(dir.path().join("b")).unwrap(), "second");
}

#[test]
fn cancellation_authority_and_racing_revisions_fail_before_any_commit() {
    use crate::tools::edit::editing::{Execution, finish_write_with};
    let (dir, ws) = workspace();
    let args = json!({"files":[{"path":"a","content":"abc"},{"path":"b","content":"def"}]});
    let deny = |_: &[std::path::PathBuf]| Err("denied".to_owned());
    assert!(
        output(
            &ws,
            &args,
            &Execution {
                authorize: Some(&deny),
                ..Default::default()
            }
        )
        .is_err()
    );
    let cancel = std::sync::atomic::AtomicBool::new(true);
    assert!(
        output(
            &ws,
            &args,
            &Execution {
                cancel: Some(&cancel),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(!dir.path().join("a").exists());
    assert!(!dir.path().join("b").exists());
    let created = run(&ws, "write", json!({"path":"a","content":"abc"}));
    assert!(!created.is_error, "{}", created.text);
    let args = json!({"files":[{"path":"b","content":"def"},{"path":"a","content":"new","expected_sha256":created.data.unwrap()["files"][0]["sha256"]}]});
    let (changes, policies) = plan(&ws, &args).unwrap();
    fs::write(dir.path().join("a"), "raced").unwrap();
    assert!(finish_write_with(&ws, changes, &Execution::default(), &policies, &args).is_err());
    assert!(!dir.path().join("b").exists());
    assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "raced");
}

#[test]
fn request_validation_and_caps_are_enforced() {
    let (dir, ws) = workspace();
    for args in [
        json!({"files":[]}),
        json!({"files":vec![json!({"path":"a","content":"x"});65]}),
        json!({"path":"a","content":"x","source":{"artifact":"artifact://x","sha256":"0".repeat(64)}}),
        json!({"path":"a","content":"x","expected_sha256":"bad"}),
        json!({"path":"a","content":"x","idempotent":"yes"}),
        json!({"files":[{"path":"a","content":"x"}],"path":"b"}),
        json!({"files":[{"path":"ssh://host/a","content":"x"}]}),
        json!({"path":"a","content":"x".repeat(1024*1024+1)}),
        json!({"files":(0..9).map(|n| json!({"path":format!("file{n}"),"content":"x".repeat(1024*1024)})).collect::<Vec<_>>()}),
    ] {
        let out = run(&ws, "write", args);
        assert!(out.is_error, "{}", out.text);
    }
    assert!(!dir.path().join("a").exists());
    assert!(!dir.path().join("file0").exists());
}

#[test]
fn compact_receipts_restore_batch_witnesses_after_resume() {
    let (dir, ws) = workspace();
    let args = json!({"files":[{"path":"a","content":"abc"},{"path":"b","content":"def"}]});
    let out = run(&ws, "write", args.clone());
    assert!(!out.is_error, "{}", out.text);
    let resumed = crate::tools::Workspace::new(dir.path()).unwrap();
    crate::tools::edit::hashline::rehydrate(&resumed, [("write", &args, out.text.as_str())]);
    let out = run(&resumed, "write", json!({"path":"b","content":"updated"}));
    assert!(!out.is_error, "{}", out.text);
}

#[test]
fn controller_batch_authorizes_all_paths_and_reports_only_actual_mutations() {
    use crate::tools::{edit::editing::Execution, integrations::ext::ToolContext};
    let (dir, ws) = workspace();
    let config = crate::config::types::Config::default();
    let deny_second = |_: &str, paths: &[std::path::PathBuf], _: bool| {
        if paths.iter().any(|path| path.ends_with("b")) {
            Err("denied b".to_owned())
        } else {
            Ok(())
        }
    };
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: Some(&deny_second),
        confirm: None,
    };
    let args = json!({"files":[{"path":"a","content":"abc"},{"path":"b","content":"def"}]});
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let denied = crate::tools::write_with_context(&ctx, &args, None, &cancel);
    assert!(denied.is_error, "{}", denied.text);
    assert!(!dir.path().join("a").exists());
    assert!(!dir.path().join("b").exists());
    assert!(
        output(
            &ws,
            &json!({"path":"a","content":"abc"}),
            &Execution::default()
        )
        .is_ok()
    );
    let authorize = crate::tools::testkit::authorize_in_workspace(&ws, "write");
    let ctx = ToolContext {
        mutation_authorizer: Some(&authorize),
        ..ctx
    };
    let args = json!({"files":[{"path":"a","content":"abc","idempotent":true},{"path":"b","content":"def"}]});
    let result = crate::tools::exec::execution::run(&cancel, || {
        crate::tools::write_with_context(&ctx, &args, None, &cancel)
    });
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(
        result.execution.changed_paths,
        vec![ws.resolve("b").unwrap()]
    );
    assert_eq!(result.data.unwrap()["files"][0]["outcome"], "unchanged");
}

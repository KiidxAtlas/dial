use super::*;
use serde_json::json;
fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "test".into(),
        name: name.into(),
        arguments: json!({}),
        malformed_arguments: false,
    }
}
#[test]
fn corrected_calls_keep_the_tool_schema_and_normalized_repeats_stay_withheld() {
    let mut checkpoint = RecoveryCheckpoint::default();
    let mut failed = call("bash");
    failed.arguments = json!({"command":"cargo check", "timeout":10});
    checkpoint.arm(&failed, "failed check");
    let mut retry = failed.clone();
    retry.arguments = json!({"command":"cargo   'check'", "timeout":20});
    assert!(checkpoint.rejection(&retry).is_some());
    retry.arguments = json!({"command":"cargo test"});
    assert!(checkpoint.rejection(&retry).is_none());
}

#[test]
fn externally_changed_source_releases_only_its_read_feedback() {
    for failed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let ws = crate::tools::Workspace::new(dir.path()).unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "original").unwrap();
        let mut checkpoint = RecoveryCheckpoint::default();
        let mut read = call("read");
        read.arguments = json!({"path":"a.txt:1-1"});
        let mut investigation =
            crate::agent::workflow::investigation::InvestigationGuard::default();
        let mut repeats = crate::agent::safety::guard::RepeatGuard::default();
        checkpoint.arm(&read, "unchanged evidence");
        checkpoint.arm(&call("bash"), "failed command");
        checkpoint.observe_at(
            &ws,
            &read,
            &crate::runtime::tool_output::ToolOutput {
                execution: Default::default(),
                data: None,
                text: "original".into(),
                is_error: failed,
            },
            &mut investigation,
            &mut repeats,
        );
        assert!(checkpoint.rejection_at(&ws, &read).is_some());
        std::fs::write(dir.path().join("unrelated.txt"), "other").unwrap();
        assert!(checkpoint.rejection_at(&ws, &read).is_some());
        std::fs::write(&path, "changed").unwrap();
        assert!(checkpoint.rejection_at(&ws, &read).is_none());
        assert!(checkpoint.rejection_at(&ws, &call("bash")).is_some());
    }
}

#[test]
fn only_meaningful_recovery_restores_tools() {
    let mut checkpoint = RecoveryCheckpoint::default();
    checkpoint.arm(&call("read"), "unchanged evidence");
    checkpoint.progress(&call("bash"), false, false, false, &[]);
    checkpoint.progress(&call("todo"), true, false, false, &[]);
    checkpoint.progress(&call("grep"), true, false, false, &[]);
    checkpoint.progress(&call("artifact_read"), true, false, false, &[]);
    checkpoint.progress(&call("edit"), true, false, false, &[]);
    let mut task = call("todo");
    task.arguments = json!({"op":"set"});
    checkpoint.progress(&task, true, true, false, &[]);
    assert!(checkpoint.rejection(&call("read")).is_some());
    let mut other = call("read");
    other.arguments = json!({"path":"another.rs"});
    assert!(checkpoint.rejection(&other).is_none());
    // A feedback entry with an unknown dependency is released by any novel successful mutation.
    checkpoint.progress(
        &call("write"),
        true,
        false,
        true,
        &[std::path::PathBuf::from("another.rs")],
    );
    assert!(checkpoint.rejection(&call("read")).is_none());
    checkpoint.arm(&call("edit"), "wrong arguments");
    task.arguments = json!({"op":"done"});
    checkpoint.progress(&task, true, true, false, &[]);
    assert!(checkpoint.rejection(&call("edit")).is_none());
    checkpoint.arm(&call("read"), "unchanged evidence");
    task.arguments = json!({"op":"block"});
    checkpoint.progress(&task, true, true, false, &[]);
    assert!(checkpoint.rejection(&call("read")).is_none());
    checkpoint.arm(&call("finish"), "invalid outcome");
    assert!(checkpoint.rejection(&call("finish")).is_none());
}

#[test]
fn known_file_feedback_is_released_only_by_a_change_to_that_file() {
    let dir = tempfile::tempdir().unwrap();
    let ws = crate::tools::Workspace::new(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta").unwrap();
    let a = ws.resolve("a.txt").unwrap();
    let b = ws.resolve("b.txt").unwrap();
    let mut read = call("read");
    read.arguments = json!({"path":"a.txt:1-1"});
    let mut write = call("write");
    write.arguments = json!({"path":"a.txt","content":"x"});
    let mut checkpoint = RecoveryCheckpoint::default();
    let mut investigation = crate::agent::workflow::investigation::InvestigationGuard::default();
    let mut repeats = crate::agent::safety::guard::RepeatGuard::default();
    let ok = || crate::runtime::tool_output::ToolOutput::ok("alpha".into());
    checkpoint.arm(&read, "unchanged evidence");
    checkpoint.observe_at(&ws, &read, &ok(), &mut investigation, &mut repeats);
    checkpoint.arm(&write, "failed write");
    checkpoint.observe_at(&ws, &write, &ok(), &mut investigation, &mut repeats);
    // A committed change to an unrelated file leaves both known-file feedbacks withheld.
    checkpoint.progress(&call("edit"), true, false, true, std::slice::from_ref(&b));
    assert!(checkpoint.rejection(&read).is_some());
    assert!(checkpoint.rejection(&write).is_some());
    // A change to a.txt releases both of them.
    checkpoint.progress(&call("edit"), true, false, true, std::slice::from_ref(&a));
    assert!(checkpoint.rejection(&read).is_none());
    assert!(checkpoint.rejection(&write).is_none());
}

#[test]
fn batched_read_feedback_expires_when_any_target_changes_or_disappears() {
    let dir = tempfile::tempdir().unwrap();
    let ws = crate::tools::Workspace::new(dir.path()).unwrap();
    let mut read = call("read");
    read.arguments = json!({"paths":["a.txt:1-1", "b.txt:1-1"]});
    let mut checkpoint = RecoveryCheckpoint::default();
    let mut investigation = crate::agent::workflow::investigation::InvestigationGuard::default();
    let mut repeats = crate::agent::safety::guard::RepeatGuard::default();
    for remove in [false, true] {
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta").unwrap();
        checkpoint.arm(&read, "unchanged evidence");
        checkpoint.observe_at(
            &ws,
            &read,
            &crate::runtime::tool_output::ToolOutput {
                text: "alpha and beta".into(),
                is_error: false,
                data: None,
                execution: Default::default(),
            },
            &mut investigation,
            &mut repeats,
        );
        assert!(checkpoint.rejection_at(&ws, &read).is_some());
        std::fs::write(dir.path().join("unrelated.txt"), "other").unwrap();
        assert!(checkpoint.rejection_at(&ws, &read).is_some());
        if remove {
            std::fs::remove_file(dir.path().join("b.txt")).unwrap();
        } else {
            std::fs::write(dir.path().join("b.txt"), "changed").unwrap();
        }
        assert!(checkpoint.rejection_at(&ws, &read).is_none());
    }
}

#[test]
fn directory_feedback_tracks_visible_changes_for_single_and_batched_reads() {
    for batched in [false, true] {
        for remove in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let ws = crate::tools::Workspace::new(dir.path()).unwrap();
            std::fs::create_dir_all(dir.path().join("src/nested")).unwrap();
            std::fs::write(dir.path().join("stable.txt"), "stable").unwrap();
            let changed = dir.path().join("src/nested/visible.rs");
            if remove {
                std::fs::write(&changed, "source").unwrap();
            }
            let mut read = call("read");
            read.arguments = if batched {
                json!({"paths":["stable.txt", "src"]})
            } else {
                json!({"path":"src"})
            };
            let mut checkpoint = RecoveryCheckpoint::default();
            let mut investigation =
                crate::agent::workflow::investigation::InvestigationGuard::default();
            let mut repeats = crate::agent::safety::guard::RepeatGuard::default();
            checkpoint.arm(&read, "unchanged listing");
            checkpoint.arm(&call("bash"), "unrelated failed command");
            checkpoint.observe_at(
                &ws,
                &read,
                &crate::runtime::tool_output::ToolOutput::ok("directory listing".into()),
                &mut investigation,
                &mut repeats,
            );
            assert!(checkpoint.rejection_at(&ws, &read).is_some());
            std::fs::write(dir.path().join("unrelated.txt"), "other").unwrap();
            assert!(checkpoint.rejection_at(&ws, &read).is_some());
            if remove {
                std::fs::remove_file(changed).unwrap();
            } else {
                std::fs::write(changed, "source").unwrap();
            }
            assert!(checkpoint.rejection_at(&ws, &read).is_none());
            assert!(checkpoint.rejection_at(&ws, &call("bash")).is_some());
        }
    }
}

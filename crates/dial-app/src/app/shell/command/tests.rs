use super::*;
use crate::app::shell::testkit::shell;
use crate::llm::provider::{Backend, ProviderKind};
use crate::ui::transcript::TranscriptCtx;

#[test]
fn permissions_explain_native_path_scope_on_add_list_remove_and_usage() {
    for command in [
        "/permissions deny read --path '*.pem'",
        "/permissions",
        "/permissions remove 1",
        "/permissions unknown",
    ] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        shell.command(&app, &mut agent, command).unwrap();
        let ctx = TranscriptCtx {
            theme: shell.themes.current(),
            ui: &shell.ui,
            tick: 0,
            tools_expanded: false,
            expand_key: "Ctrl+O",
        };
        let text = shell
            .transcript
            .document(400, &ctx)
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Path rules apply only to native",
            "does not block bash cat, grep, glob",
            "Deny those tools explicitly",
            "filesystem confinement",
        ] {
            assert!(
                text.contains(expected),
                "{command}: missing {expected:?}: {text}"
            );
        }
        if command.contains("deny read") {
            assert_eq!(agent.permission_rules().len(), 1);
            assert_eq!(agent.permission_rules()[0].paths, ["*.pem"]);
            assert_eq!(agent.permission_rules()[0].tool, "read");
        }
    }
}

#[test]
fn permission_removal_picker_shows_rule_paths_and_scope_limitation() {
    let (_home, _project, app, mut shell) = shell();
    let mut agent = permission_agent(&app);
    shell
        .command(&app, &mut agent, "/permissions deny read --path '*.pem'")
        .unwrap();
    let picker = shell.picker(&app, &agent, "permissions", "remove").unwrap();
    assert!(picker.items[0].detail.contains("--path *.pem"));
    assert!(
        picker
            .summary
            .contains("does not block bash cat, grep, glob")
    );
    assert!(picker.summary.contains("filesystem confinement"));
}

fn permission_agent(app: &App) -> Agent<'_> {
    // No request is sent: the backend only supplies the agent's endpoint identity.
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "fixture".into(),
            api_key: None,
        },
        "unused-python".into(),
        1,
    );
    Agent::interactive(
        &app.config,
        backend,
        app.layout.clone(),
        Session::ephemeral(),
        None,
        None,
        app.interrupt.clone(),
    )
    .unwrap()
}

#[test]
fn undo_keeps_conversation_on_partial_restore_and_both_commands_show_warnings() {
    for (command, failure) in [
        ("/undo", "oversized"),
        ("/rollback", "oversized"),
        ("/undo", "restore"),
        ("/rollback", "restore"),
    ] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        let cp = agent.checkpoints().unwrap().clone();
        let session = agent.session().id().to_owned();
        let turn = cp.begin_conversation_turn("partial", &session).unwrap();
        let path = app.layout.project_root.join("file");
        std::fs::write(&path, "original").unwrap();
        if failure == "oversized" {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(crate::storage::checkpoint::MAX_FILE_BYTES + 1)
                .unwrap();
        }
        cp.before_write(&path).unwrap();
        if failure == "restore" {
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
        agent.session_mut().append(serde_json::json!({"role":"user","content":"change file", "dial_checkpoint":{"session":session,"turn":turn}})).unwrap();
        let before = agent.session().messages();
        shell.command(&app, &mut agent, command).unwrap();
        assert_eq!(agent.session().messages(), before, "{command}");
        let ctx = TranscriptCtx {
            theme: shell.themes.current(),
            ui: &shell.ui,
            tick: 0,
            tools_expanded: false,
            expand_key: "Ctrl+O",
        };
        let text = shell
            .transcript
            .document(400, &ctx)
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("rollback stopped"),
            "{command}, {failure}: {text}"
        );
        if failure == "oversized" {
            assert!(
                text.contains("too large to checkpoint") && text.contains("not restored"),
                "{command}: {text}"
            );
        }
        if command == "/undo" {
            assert!(text.contains("conversation kept"), "{text}");
        }
    }
}

#[cfg(unix)]
#[test]
fn undo_branch_veto_runs_before_restoration() {
    let (_home, _project, mut app, mut shell) = shell();
    app.config.hooks.push(crate::config::types::HookConfig {
        event: "session_before_branch".into(),
        matcher: None,
        command: vec!["sh".into(), "-c".into(), "echo veto >&2; exit 2".into()],
        timeout_secs: 5,
        fail_closed: None,
    });
    let mut agent = permission_agent(&app);
    let cp = agent.checkpoints().unwrap().clone();
    let session = agent.session().id().to_owned();
    let path = app.layout.project_root.join("file");
    std::fs::write(&path, "original").unwrap();
    let turn = cp.begin_conversation_turn("edit", &session).unwrap();
    cp.before_write(&path).unwrap();
    std::fs::write(&path, "edited").unwrap();
    agent.session_mut().append(serde_json::json!({"role":"user","content":"edit", "dial_checkpoint":{"session":session,"turn":turn}})).unwrap();
    let before = agent.session().messages();
    let checkpoints = cp.list();
    shell.command(&app, &mut agent, "/undo").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
    assert_eq!(cp.list(), checkpoints);
    assert_eq!(agent.session().messages(), before);
}

#[cfg(unix)]
#[test]
fn undo_retry_finishes_without_replaying_newer_restore_or_gating_twice() {
    let (_home, _project, mut app, mut shell) = shell();
    // Branch summarization must fail locally, without any model request.
    app.config.provider = Some("mlx".into());
    app.config.endpoint = None;
    app.config.hooks.push(crate::config::types::HookConfig {
        event: "session_before_branch".into(),
        matcher: None,
        command: vec!["sh".into(), "-c".into(), "echo gate >> gate-count".into()],
        timeout_secs: 5,
        fail_closed: None,
    });
    let mut agent = permission_agent(&app);
    let cp = agent.checkpoints().unwrap().clone();
    let session = agent.session().id().to_owned();
    let older = app.layout.project_root.join("older");
    let newer = app.layout.project_root.join("newer");
    for (path, label) in [(&older, "older"), (&newer, "newer")] {
        std::fs::write(path, format!("original {label}")).unwrap();
        let turn = cp.begin_conversation_turn(label, &session).unwrap();
        cp.before_write(path).unwrap();
        std::fs::write(path, format!("changed {label}")).unwrap();
        agent.session_mut().append(serde_json::json!({"role":"user","content":label, "dial_checkpoint":{"session":session,"turn":turn}})).unwrap();
    }
    std::fs::remove_file(&older).unwrap();
    std::fs::create_dir(&older).unwrap();
    let before = agent.session().messages();
    shell.command(&app, &mut agent, "/undo 2").unwrap();
    assert_eq!(agent.session().messages(), before);
    assert_eq!(std::fs::read_to_string(&newer).unwrap(), "original newer");
    std::fs::remove_dir(&older).unwrap();
    std::fs::write(&newer, "after completed restore").unwrap();
    shell.command(&app, &mut agent, "/undo 2").unwrap();
    assert!(agent.session().messages().is_empty());
    assert_eq!(std::fs::read_to_string(&older).unwrap(), "original older");
    assert_eq!(
        std::fs::read_to_string(&newer).unwrap(),
        "after completed restore"
    );
    assert_eq!(
        std::fs::read_to_string(app.layout.project_root.join("gate-count")).unwrap(),
        "gate\ngate\n"
    );
    assert!(cp.list().is_empty());
    let checkpoint_dir = app.layout.checkpoints_dir().join(session);
    assert!(
        std::fs::read_dir(checkpoint_dir)
            .unwrap()
            .all(|entry| !entry.unwrap().file_type().unwrap().is_dir())
    );
}

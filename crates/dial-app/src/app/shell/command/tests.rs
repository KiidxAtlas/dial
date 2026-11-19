use super::*;
use crate::app::shell::testkit::shell;
use crate::llm::provider::{Backend, ProviderKind};
use crate::ui::transcript::TranscriptCtx;

#[test]
fn everyday_queue_refuses_trailing_input_without_removing_a_message() {
    let (_home, _project, app, mut shell) = shell();
    let mut agent = permission_agent(&app);
    shell.composer.restore_pending_messages(vec!["keep".into()]);
    shell
        .command(&app, &mut agent, "/queue remove 1 2")
        .unwrap();
    assert_eq!(shell.composer.take_follow_up().as_deref(), Some("keep"));
}

#[test]
fn malformed_session_controls_keep_work_running_and_do_not_change_session() {
    for line in [
        "/new please",
        "/clear drafts",
        "/q later",
        "/undo files typo",
        "/rollback 0",
    ] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        let id = agent.session().id().to_owned();
        shell.composer.set_draft("unfinished thought");
        assert!(
            !shell.route_busy_command(&app, line.into()),
            "{line} stopped the turn"
        );
        assert!(
            shell.startup.is_none() && shell.deferred.is_empty(),
            "{line} was scheduled"
        );
        assert!(matches!(
            shell.command(&app, &mut agent, line).unwrap(),
            Step::Done
        ));
        assert_eq!(agent.session().id(), id, "{line}");
        assert_eq!(shell.composer.draft(), "unfinished thought", "{line}");
        assert!(transcript_text(&shell).contains("Usage:"), "{line}");
    }
}

#[test]
fn everyday_help_delivers_the_advertised_guide_and_command_idle_and_busy() {
    for busy in [false, true] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        if busy {
            assert!(!shell.route_busy_command(&app, "/help models".into()));
        } else {
            shell.command(&app, &mut agent, "/help models").unwrap();
        }
        let text = transcript_text(&shell);
        assert!(text.contains("also: dial models"), "busy={busy}: {text}");
        assert!(
            text.contains("docs/terminal-models.md"),
            "busy={busy}: {text}"
        );
    }
}

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
    for command in ["/undo", "/rollback"] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        let cp = agent.checkpoints().unwrap().clone();
        let session = agent.session().id().to_owned();
        let turn = cp.begin_conversation_turn("partial", &session).unwrap();
        let path = app.layout.project_root.join("file");
        std::fs::write(&path, "original").unwrap();
        cp.before_write(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        agent.session_mut().append(serde_json::json!({"role":"user","content":"change file", "dial_checkpoint":{"session":session,"turn":turn}})).unwrap();
        let before = agent.session().messages();
        shell.command(&app, &mut agent, command).unwrap();
        assert_eq!(agent.session().messages(), before, "{command}");
        let text = transcript_text(&shell);
        assert!(text.contains("rollback stopped"), "{command}: {text}");
        if command == "/undo" {
            assert!(text.contains("conversation kept"), "{text}");
        }
    }
}

/// A file too large to checkpoint cannot come back; both commands name it and finish instead
/// of stopping at it forever.
#[test]
fn oversized_files_are_named_without_blocking_undo_or_rollback() {
    for command in ["/undo", "/rollback"] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        let cp = agent.checkpoints().unwrap().clone();
        let session = agent.session().id().to_owned();
        let turn = cp.begin_conversation_turn("big", &session).unwrap();
        let path = app.layout.project_root.join("file");
        std::fs::write(&path, "original").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(crate::storage::checkpoint::MAX_FILE_BYTES + 1)
            .unwrap();
        cp.before_write(&path).unwrap();
        agent.session_mut().append(serde_json::json!({"role":"user","content":"change file", "dial_checkpoint":{"session":session,"turn":turn}})).unwrap();
        let before = agent.session().messages();
        shell.command(&app, &mut agent, command).unwrap();
        let text = transcript_text(&shell);
        assert!(
            text.contains("too large to checkpoint") && text.contains("keeps the turn's content"),
            "{command}: {text}"
        );
        assert!(!text.contains("rollback stopped"), "{command}: {text}");
        if command == "/undo" {
            assert_ne!(
                agent.session().messages(),
                before,
                "the conversation is rewound"
            );
        }
    }
}

fn transcript_text(shell: &Shell) -> String {
    let ctx = TranscriptCtx {
        theme: shell.themes.current(),
        ui: &shell.ui,
        tick: 0,
        tools_expanded: false,
        expand_key: "Ctrl+O",
    };
    shell
        .transcript
        .document(400, &ctx)
        .0
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
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

#[test]
fn compaction_cancel_precedes_compute_busy_and_history_mutation() {
    let (_home, _project, app, mut shell) = shell();
    let mut agent = permission_agent(&app);
    agent
        .session_mut()
        .append(serde_json::json!({"role":"user","content":"history remains"}))
        .unwrap();
    let before = agent.session().messages();
    shell.composer.set_busy(true);
    shell.composer.set_draft("queued input");
    shell.composer.submit_voice();
    shell.composer.set_busy(false);
    shell.composer.set_draft("unsent input");
    let queued = shell.composer.queued_messages();
    let phase = shell.status.phase;
    let mut reviewed = false;
    shell
        .compact_command_review(&app, &mut agent, "soft", |shell, text| {
            reviewed = true;
            assert!(text.contains("Eligible older history"));
            assert!(text.contains("Cancel leaves history unchanged"));
            assert_eq!(shell.status.phase, phase);
            assert_eq!(shell.composer.draft(), "unsent input");
            false
        })
        .unwrap();
    assert!(reviewed);
    assert_eq!(agent.session().messages(), before);
    assert_eq!(shell.composer.queued_messages(), queued);
    assert_eq!(shell.composer.draft(), "unsent input");
    assert_eq!(shell.status.phase, phase);
}

#[test]
fn context_command_counts_pending_input_without_consuming_it() {
    let (_home, _project, app, mut shell) = shell();
    let agent = permission_agent(&app);
    shell.composer.set_busy(true);
    shell.composer.set_draft("queued input");
    shell.composer.submit_voice();
    shell.composer.set_busy(false);
    shell.composer.set_draft("unsent input");
    let queued = shell.composer.queued_messages();
    shell.context_command(&agent).unwrap();
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
    assert!(text.contains("2 queued/draft messages included"), "{text}");
    assert!(text.contains("Last sent request: none yet"), "{text}");
    assert_eq!(shell.composer.queued_messages(), queued);
    assert_eq!(shell.composer.draft(), "unsent input");
    assert!(agent.session().messages().is_empty());
}

#[test]
fn cache_line_reports_the_hit_rate_only_from_reported_cache_usage() {
    let counts = |input, cache_read, cache_reports, reports| usage::Counts {
        input,
        cache_read,
        cache_reported_responses: cache_reports,
        reported_responses: reports,
        ..usage::Counts::default()
    };
    assert_eq!(
        cache_line(&counts(1000, 750, 4, 4)),
        "cache hit rate: 75% of prompt tokens"
    );
    assert_eq!(
        cache_line(&counts(1000, 250, 1, 4)),
        "cache hit rate: at least 25% of prompt tokens (cache reported on 1 of 4 responses)"
    );
    // Absent cache fields are unknown, never a 0% hit rate.
    assert_eq!(
        cache_line(&counts(1000, 0, 0, 4)),
        "cache hit rate: not reported by the provider"
    );
    assert_eq!(
        cache_line(&counts(0, 0, 1, 1)),
        "cache hit rate: unknown (no prompt tokens reported)"
    );
}

#[test]
fn prefix_reset_line_names_each_cause_and_is_absent_without_resets() {
    let mut resets = std::collections::BTreeMap::new();
    assert_eq!(prefix_reset_line(&resets), None);
    resets.insert("tools_changed", 2);
    resets.insert("model_changed", 1);
    let line = prefix_reset_line(&resets).unwrap();
    assert!(
        line.starts_with(
            "Prompt cache restarts (main agent): model switched 1 · tool list changed 2."
        ),
        "{line}"
    );
}

#[test]
fn permissions_why_lists_recent_policy_decisions_newest_first() {
    let (_home, _project, app, mut shell) = shell();
    let mut agent = permission_agent(&app);
    let shown = |shell: &mut Shell| {
        let ctx = TranscriptCtx {
            theme: shell.themes.current(),
            ui: &shell.ui,
            tick: 0,
            tools_expanded: false,
            expand_key: "Ctrl+O",
        };
        shell
            .transcript
            .document(400, &ctx)
            .0
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    shell.command(&app, &mut agent, "/permissions why").unwrap();
    assert!(shown(&mut shell).contains("No tool call has passed the approval policy"));
    shell.policy_log.push_back("read: first".into());
    shell.policy_log.push_back("bash: second".into());
    shell.command(&app, &mut agent, "/permissions why").unwrap();
    let text = shown(&mut shell);
    let (second, first) = (
        text.find("bash: second").unwrap(),
        text.find("read: first").unwrap(),
    );
    assert!(second < first, "{text}");
}

/// A typo in a closed-grammar command is answered at once on both routes. It never stops the
/// turn, waits in the after-turn queue to fail later, or changes the session it was typed in.
#[test]
fn malformed_closed_grammar_commands_answer_at_once_idle_and_busy() {
    for (line, usage) in [
        ("/plan maybe", "Usage: /plan [on|off]"),
        ("/mode plan maybe", "Usage: /plan [on|off]"),
        (
            "/approval nope",
            "Usage: /approval [ask|write|auto|readonly]",
        ),
        ("/laya nope", "Usage: /laya [status|check|start|on|off]"),
        ("/tree x", "Usage: /tree"),
        ("/fork typo", "Usage: /fork"),
        ("/cancel", "Usage: /cancel <id>"),
        ("/message", "Usage: /message <id> <text>"),
        ("/message task-9", "Usage: /message <id> <text>"),
    ] {
        for busy in [false, true] {
            let (_home, _project, app, mut shell) = shell();
            let mut agent = permission_agent(&app);
            let id = agent.session().id().to_owned();
            let (plan, approval) = (agent.plan_mode(), agent.approval());
            if busy {
                assert!(
                    !shell.route_busy_command(&app, line.into()),
                    "{line} stopped the turn"
                );
                assert!(
                    shell.startup.is_none() && shell.deferred.is_empty(),
                    "{line} was scheduled"
                );
            } else {
                assert!(matches!(
                    shell.command(&app, &mut agent, line).unwrap(),
                    Step::Done
                ));
            }
            let text = transcript_text(&shell);
            assert!(text.contains(usage), "{line} busy={busy}: {text}");
            assert_eq!(agent.session().id(), id, "{line} busy={busy}");
            assert_eq!(agent.plan_mode(), plan, "{line} busy={busy}");
            assert_eq!(agent.approval(), approval, "{line} busy={busy}");
        }
    }
}

/// The syntax check must not swallow the forms it guards.
#[test]
fn well_formed_closed_grammar_commands_still_run() {
    let (_home, _project, app, mut shell) = shell();
    let mut agent = permission_agent(&app);
    shell.command(&app, &mut agent, "/plan on").unwrap();
    assert!(agent.plan_mode());
    shell.command(&app, &mut agent, "/mode plan off").unwrap();
    assert!(!agent.plan_mode());
    shell
        .command(&app, &mut agent, "/approval readonly")
        .unwrap();
    assert_eq!(agent.approval(), Approval::Readonly);
    assert!(!transcript_text(&shell).contains("Usage:"));
    // While busy they keep their declared scheduling: /plan stops the turn, /approval waits.
    assert!(shell.route_busy_command(&app, "/plan on".into()));
    assert_eq!(shell.startup.as_deref(), Some("/plan on"));
    assert!(!shell.route_busy_command(&app, "/approval ask".into()));
    assert_eq!(shell.deferred.len(), 1);
}

/// `/undo` and `/undo files` describe a file restore the same way, naming the files instead of
/// printing counts in one command and a debug list in the other.
#[test]
fn file_restore_reads_the_same_for_undo_and_undo_files() {
    for command in ["/undo", "/undo files"] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        let cp = agent.checkpoints().unwrap().clone();
        let session = agent.session().id().to_owned();
        let turn = cp.begin_conversation_turn("edit", &session).unwrap();
        let kept = app.layout.project_root.join("kept.txt");
        let created = app.layout.project_root.join("created.txt");
        std::fs::write(&kept, "original").unwrap();
        cp.before_write(&kept).unwrap();
        std::fs::write(&kept, "changed").unwrap();
        cp.before_write(&created).unwrap();
        std::fs::write(&created, "new").unwrap();
        agent.session_mut().append(serde_json::json!({"role":"user","content":"change files", "dial_checkpoint":{"session":session,"turn":turn}})).unwrap();
        shell.command(&app, &mut agent, command).unwrap();
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "original");
        assert!(!created.exists(), "{command}");
        let text = transcript_text(&shell);
        assert!(
            text.contains("Restored 1 file: kept.txt"),
            "{command}: {text}"
        );
        assert!(
            text.contains("Removed 1 file the undone work created: created.txt"),
            "{command}: {text}"
        );
        assert!(!text.contains("[\""), "{command}: {text}");
    }
}

/// An empty conversation has nothing to rewind; say so instead of reporting zero messages.
#[test]
fn undo_on_an_empty_conversation_says_there_is_nothing_to_undo() {
    let (_home, _project, app, mut shell) = shell();
    let mut agent = permission_agent(&app);
    shell.command(&app, &mut agent, "/undo").unwrap();
    let text = transcript_text(&shell);
    assert!(text.contains("Nothing to undo"), "{text}");
    assert!(!text.contains("dropped 0"), "{text}");
}

/// Custom commands are listed by one owner, so the idle and busy routes cannot drift.
#[test]
fn custom_command_listing_matches_idle_and_busy() {
    let mut texts = Vec::new();
    for busy in [false, true] {
        let (_home, _project, app, mut shell) = shell();
        let mut agent = permission_agent(&app);
        if busy {
            assert!(!shell.route_busy_command(&app, "/commands".into()));
        } else {
            shell.command(&app, &mut agent, "/commands").unwrap();
        }
        texts.push(transcript_text(&shell));
    }
    assert_eq!(texts[0], texts[1]);
    assert!(texts[0].contains("/review"), "{}", texts[0]);
}

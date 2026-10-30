use super::*;

fn write(path: impl AsRef<Path>, content: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}
fn layout(project: &Path, user: &Path) -> Layout {
    Layout::with_user_dir(project, user.to_path_buf()).unwrap()
}
fn sources(catalog: &Catalog) -> Vec<&str> {
    catalog
        .rules
        .iter()
        .map(|rule| rule.source.as_str())
        .collect()
}

#[test]
fn edit_paths_include_native_sloppy_targets_and_structured_renames() {
    let root = Path::new("/project");
    let sloppy = serde_json::json!({"input": "*** Edit File: \"source file.rs\" all\n*** Find\nold\n*** Replace\nnew"});
    assert_eq!(
        argument_paths(root, "edit", &sloppy),
        BTreeSet::from(["source file.rs".into()])
    );
    let patch =
        serde_json::json!({"files": [{"path":"old.rs", "rename":"new.rs", "diff":"-old\n+new"}]});
    assert_eq!(
        argument_paths(root, "edit", &patch),
        BTreeSet::from(["old.rs".into(), "new.rs".into()])
    );
}

#[test]
fn discovers_windsurf_rules() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let project = base.path().join("project");
    write(
        home.join(".codeium/windsurf/memories/global_rules.md"),
        "---\nalwaysApply: true\n---\nuser windsurf",
    );
    write(
        project.join(".windsurf/rules/ts.md"),
        "---\ntrigger: glob\nglobs: \"*.ts\"\n---\nts rule",
    );
    write(
        project.join(".windsurf/rules/nested/deep.md"),
        "---\nglobs: \"*.rs\"\n---\nnot top level",
    );
    write(
        project.join(".windsurf/rules/manual.md"),
        "---\ntrigger: manual\ndescription: manual\n---\nmanual",
    );
    let layout = layout(&project, &base.path().join("dial-user"));

    // Foreign user providers are opt-in: the home-level rule is absent by default.
    let off = Catalog::discover_with(&layout, &project, Some(&home), &[], &[]);
    assert_eq!(sources(&off), ["project:windsurf:.windsurf/rules/ts.md"]);

    let on = Catalog::discover_with(&layout, &project, Some(&home), &["windsurf"], &[]);
    assert_eq!(
        sources(&on),
        [
            "user:windsurf:.codeium/windsurf/memories/global_rules.md",
            "project:windsurf:.windsurf/rules/ts.md",
        ]
    );
    // A `manual` trigger has no globs or alwaysApply, so Dial refuses it as before.
    assert!(on.warnings.iter().any(|w| w.contains("manual.md")));
}

#[test]
fn discovers_cline_rules() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let repo = base.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    let project = repo.join("pkg");
    fs::create_dir_all(&project).unwrap();
    write(
        repo.join(".clinerules/style.md"),
        "---\nglobs: \"*.rs\"\n---\nstyle",
    );
    write(repo.join(".clinerules/notes.txt"), "ignored");
    let layout = layout(&project, &base.path().join("dial-user"));

    // The walk reaches the git root, where the `.clinerules` directory lives.
    let dir = Catalog::discover_with(&layout, &project, Some(&home), &[], &[]);
    assert_eq!(sources(&dir), ["project:cline:.clinerules/style.md"]);

    // The nearest `.clinerules` file wins over the directory above it.
    write(
        project.join(".clinerules"),
        "---\nglobs: \"*.ts\"\n---\nnearest",
    );
    let file = Catalog::discover_with(&layout, &project, Some(&home), &[], &[]);
    assert_eq!(sources(&file), ["project:cline:.clinerules"]);
    assert_eq!(file.rules[0].body.trim(), "nearest");
}

#[test]
fn portable_rules_precedence() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let project = base.path().join("project");
    write(
        home.join(".codeium/windsurf/memories/global_rules.md"),
        "---\nalwaysApply: true\n---\nuser",
    );
    write(
        project.join(".windsurf/rules/global_rules.md"),
        "---\nalwaysApply: true\n---\nproject",
    );
    let layout = layout(&project, &base.path().join("dial-user"));

    // Same provider and name: the project file shadows the user file.
    let got = Catalog::discover_with(&layout, &project, Some(&home), &["windsurf"], &[]);
    assert_eq!(
        sources(&got),
        ["project:windsurf:.windsurf/rules/global_rules.md"]
    );
    assert!(got.rules[0].body.contains("project"));
    assert!(!got.rules[0].body.contains("user"));
}

#[cfg(unix)]
#[test]
fn refuses_symlinked_windsurf_rule_outside_root() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let project = base.path().join("project");
    write(
        base.path().join("secret.md"),
        "---\nalwaysApply: true\n---\nsecret",
    );
    fs::create_dir_all(project.join(".windsurf/rules")).unwrap();
    std::os::unix::fs::symlink(
        base.path().join("secret.md"),
        project.join(".windsurf/rules/leak.md"),
    )
    .unwrap();
    let layout = layout(&project, &base.path().join("dial-user"));

    let got = Catalog::discover_with(&layout, &project, Some(&home), &["windsurf"], &[]);
    assert!(got.rules.is_empty());
    assert!(got.warnings.iter().any(|w| w.contains("leak.md")));
}

#[test]
fn copilot_instruction_dirs_load_apply_to_rules_as_user_rules() {
    let base = tempfile::tempdir().unwrap();
    let home = base.path().join("home");
    let project = base.path().join("project");
    fs::create_dir_all(&project).unwrap();
    let ext = base.path().join("ext");
    write(
        ext.join(".github/instructions/rust.instructions.md"),
        "---\napplyTo: \"**/*.rs\"\n---\ncopilot rule",
    );
    let ext = fs::canonicalize(&ext).unwrap();
    let layout = layout(&project, &base.path().join("dial-user"));
    let got = Catalog::discover_with(&layout, &project, Some(&home), &[], &[ext]);
    let rule = got
        .rules
        .iter()
        .find(|r| r.body.trim() == "copilot rule")
        .expect("copilot instruction dir rule is loaded");
    assert_eq!(rule.path_patterns, ["**/*.rs"]);
}

#[test]
fn legacy_windsurfrules_file_loads_from_project_root() {
    let base = tempfile::tempdir().unwrap();
    let project = base.path().join("project");
    write(project.join(".windsurfrules"), "legacy windsurf rule");
    let layout = layout(&project, &base.path().join("dial-user"));
    let got = Catalog::discover_with(&layout, &project, None, &[], &[]);
    assert!(
        got.rules.iter().any(
            |r| r.source.ends_with(".windsurfrules") && r.body.trim() == "legacy windsurf rule"
        ),
        "{:?}",
        sources(&got)
    );
}

#[test]
fn completed_tool_replaces_fragments_without_resetting_other_evidence() {
    let base = tempfile::tempdir().unwrap();
    let project = base.path().join("project");
    write(
        project.join(".dial/rules/text.md"),
        "---\ncondition: before-after\nscope: text\n---\nguard",
    );
    write(
        project.join(".dial/rules/tool.md"),
        "---\ncondition: secret\nscope: tool:read\n---\nguard",
    );
    let layout = layout(&project, &base.path().join("home"));
    let catalog = Catalog::discover_with(&layout, &project, None, &[], &[]);
    let state = State::default();
    let mut monitor = catalog.monitor(&project, &state, "main");
    assert!(monitor.text("before-", false));
    assert!(monitor.tool(0, "read", "{\"path\":"));
    assert!(monitor.tool(1, "read", "{\"path\":\"sec"));
    let call = ToolCall {
        id: "c0".into(),
        name: "read".into(),
        arguments: serde_json::json!({"path":"public.txt"}),
        malformed_arguments: false,
    };
    assert!(monitor.completed_tool(0, &call));
    assert_eq!(monitor.tools[&0].1, call.arguments.to_string());
    assert_eq!(
        monitor.bytes,
        monitor.text.len()
            + monitor
                .tools
                .values()
                .map(|(_, value)| value.len())
                .sum::<usize>()
    );
    assert!(
        !monitor.text("after", false),
        "text evidence survives call completion"
    );
    assert!(
        !monitor.completed_tool(2, &call),
        "an existing hit cannot be cleared"
    );

    monitor.restart();
    assert!(monitor.tool(1, "read", "{\"path\":\"sec"));
    assert!(monitor.completed_tool(0, &call));
    assert!(
        !monitor.tool(1, "", "ret.txt\"}"),
        "other call evidence survives completion"
    );
}

#[test]
fn completed_tool_keeps_cumulative_coverage_limits_and_latched_failure() {
    let base = tempfile::tempdir().unwrap();
    let project = base.path().join("project");
    write(
        project.join(".dial/rules/guard.md"),
        "---\ncondition: never-match\nscope: [text, thinking, tool:read]\n---\nguard",
    );
    let layout = layout(&project, &base.path().join("home"));
    let catalog = Catalog::discover_with(&layout, &project, None, &[], &[]);
    let state = State::default();
    let mut monitor = catalog.monitor(&project, &state, "main");
    assert!(monitor.text(&"x".repeat(STREAM_BYTES), false));
    assert!(monitor.text(&"y".repeat(STREAM_BYTES), true));
    let call = ToolCall {
        id: "c0".into(),
        name: "read".into(),
        arguments: serde_json::json!({"path":"public.txt"}),
        malformed_arguments: false,
    };
    assert!(!monitor.completed_tool(0, &call));
    let failure = monitor.failure.clone();
    assert!(failure.is_some());
    assert_eq!(monitor.bytes, TOTAL_BYTES);
    assert!(!monitor.completed_tool(0, &call));
    assert_eq!(monitor.failure, failure);
}

fn rule(frontmatter: &str) -> Catalog {
    parse_candidate(
        &format!("---\n{frontmatter}\n---\nAvoid forbidden.\n"),
        "t.md",
    )
    .unwrap()
}

#[test]
fn interrupt_mode_routes_matches_to_abort_or_deferred_reminder() {
    let root = Path::new("/project");
    // (interruptMode, match from a tool stream, aborts the inference)
    for (mode, tool, aborts) in [
        ("always", false, true),
        ("always", true, true),
        ("prose-only", false, true),
        ("prose-only", true, false),
        ("tool-only", false, false),
        ("tool-only", true, true),
        ("never", false, false),
        ("never", true, false),
    ] {
        let catalog = rule(&format!("condition: forbidden\ninterruptMode: {mode}"));
        let state = State::default();
        let mut monitor = catalog.monitor(root, &state, "main");
        let proceeds = if tool {
            monitor.tool(0, "bash", r#"{"command":"forbidden"}"#)
        } else {
            monitor.text("this is forbidden", false)
        };
        let case = format!("{mode} tool={tool}");
        assert_eq!(proceeds, !aborts, "{case}");
        assert_eq!(monitor.hit.is_some(), aborts, "{case}");
        assert_eq!(monitor.deferred.len(), usize::from(!aborts), "{case}");
        // A deferred rule is recorded once however often the stream repeats it.
        if !aborts && !tool {
            assert!(monitor.text(" and forbidden again", false), "{case}");
            assert_eq!(monitor.deferred.len(), 1, "{case}");
        }
        let mut journal = State::default();
        assert_eq!(
            journal.remember_deferred(&monitor.deferred, "main"),
            Ok(!aborts),
            "{case}"
        );
        assert_eq!(journal.reminders.len(), usize::from(!aborts), "{case}");
    }
}

#[test]
fn repeat_policy_rearms_a_fired_rule_after_the_configured_turns() {
    let root = Path::new("/project");
    for (repeat_after_turns, armed_by_turn) in [
        (0, [true, false, false, false]),
        (2, [true, false, true, false]),
    ] {
        let mut catalog = rule("condition: forbidden");
        catalog.repeat_after_turns = repeat_after_turns;
        let mut state = State::default();
        for (turn, armed) in armed_by_turn.into_iter().enumerate() {
            state.begin(root, "");
            let mut monitor = catalog.monitor(root, &state, "main");
            let case = format!("repeat_after_turns={repeat_after_turns} turn={}", turn + 1);
            assert_eq!(monitor.rules.len(), usize::from(armed), "{case}");
            if armed {
                assert!(!monitor.text("forbidden", false), "{case}");
                let hit = monitor.hit.take().unwrap();
                state.remember(&hit, "main").unwrap();
            }
        }
    }
}

#[test]
fn question_rules_are_judged_not_streamed_and_never_interrupt() {
    let catalog = rule("question: Does the reply claim tests passed without running them?");
    assert!(catalog.rules.iter().all(|rule| rule.question.is_some()));
    let state = State::default();
    assert!(
        catalog
            .monitor(Path::new("/project"), &state, "main")
            .rules
            .is_empty()
    );
    let refused = parse_candidate(
        "---\nquestion: Is it wrong?\ninterruptMode: never\n---\nBody.\n",
        "t.md",
    )
    .unwrap_err();
    assert!(refused.contains("never interrupt"), "{refused}");
    let refused = parse_candidate(
        "---\ninterruptMode: never\nglobs: \"*.rs\"\n---\nBody.\n",
        "t.md",
    )
    .unwrap_err();
    assert!(refused.contains("condition or astCondition"), "{refused}");
}

#[test]
fn judged_candidates_follow_scope_prefilter_and_repeat_policy() {
    let root = Path::new("/project");
    let reply = |text: &str, tool: Option<(&str, serde_json::Value)>| crate::llm::Reply {
        text: text.into(),
        tool_calls: tool
            .map(|(name, arguments)| {
                vec![ToolCall {
                    id: "c1".into(),
                    name: name.into(),
                    arguments,
                    malformed_arguments: false,
                }]
            })
            .unwrap_or_default(),
        ..Default::default()
    };
    let question = "question: Does it claim success without evidence?";
    // (frontmatter, reply, judged content when a candidate)
    for (frontmatter, reply, content) in [
        (
            format!("{question}\nscope: text"),
            reply("Done, all green.", None),
            Some("assistant text:\nDone, all green."),
        ),
        (
            format!("{question}\nscope: text"),
            reply("  ", Some(("bash", serde_json::json!({"command":"ls"})))),
            None,
        ),
        (
            format!("{question}\ncondition: green\nscope: text"),
            reply("Done, all passing.", None),
            None,
        ),
        (
            format!("{question}\nscope: tool:bash"),
            reply("", Some(("bash", serde_json::json!({"command":"ls"})))),
            Some("tool call bash: {\"command\":\"ls\"}"),
        ),
    ] {
        let catalog = rule(&frontmatter);
        let mut state = State::default();
        let candidates = catalog.judged_candidates(root, &state, "main", &reply);
        assert_eq!(
            candidates.first().map(|c| c.content.as_str()),
            content,
            "{frontmatter}"
        );
        if let Some(candidate) = candidates.first() {
            state.remember(&candidate.hit, "main").unwrap();
            assert!(
                catalog
                    .judged_candidates(root, &state, "main", &reply)
                    .is_empty(),
                "a fired judged rule stays quiet: {frontmatter}"
            );
        }
    }
}

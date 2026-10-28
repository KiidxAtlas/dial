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

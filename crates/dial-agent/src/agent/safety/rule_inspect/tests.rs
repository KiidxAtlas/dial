use super::*;

fn workspace() -> (tempfile::TempDir, std::path::PathBuf, Layout, Config) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let rules = project.join(".dial/rules");
    std::fs::create_dir_all(&rules).unwrap();
    for (name, text) in [
        (
            "stop.md",
            "---\ncondition: rm -rf\n---\nNever delete recursively.\n",
        ),
        (
            "remind.md",
            "---\ncondition: TODO\ninterruptMode: never\nscope: text\n---\nTrack TODOs.\n",
        ),
        (
            "judged.md",
            "---\nquestion: Does it claim success without evidence?\nscope: text\n---\nShow evidence.\n",
        ),
        ("style.md", "---\nglobs: \"*.rs\"\n---\nUse rustfmt.\n"),
        (
            "broken.md",
            "---\ninterruptMode: sometimes\ncondition: x\n---\nBody.\n",
        ),
    ] {
        std::fs::write(rules.join(name), text).unwrap();
    }
    let layout = Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    let config = Config::default();
    (temp, project, layout, config)
}

#[test]
fn list_reports_triggers_modes_and_refused_files() {
    let (_temp, project, layout, config) = workspace();
    let listing = list(&layout, &project, &config);
    let summary = |file: &str| {
        listing
            .rules
            .iter()
            .find(|rule| rule.source.ends_with(file))
            .unwrap_or_else(|| panic!("{file} not listed: {listing:?}"))
    };
    assert_eq!(summary("stop.md").triggers, ["condition"]);
    assert_eq!(summary("stop.md").interrupt_mode, "always");
    assert_eq!(summary("remind.md").interrupt_mode, "never");
    assert_eq!(summary("judged.md").triggers, ["question"]);
    assert_eq!(summary("style.md").triggers, ["globs"]);
    assert_eq!(listing.rules.len(), 4);
    assert!(
        listing
            .warnings
            .iter()
            .any(|warning| warning.contains("broken.md") && warning.contains("interruptMode")),
        "{:?}",
        listing.warnings
    );
}

#[test]
fn test_reports_every_firing_rule_with_its_trigger() {
    let (_temp, project, layout, config) = workspace();
    let source = |m: &ProbeMatch| m.source.rsplit('/').next().unwrap().to_owned();
    // Text: both stream rules match; only `stop` interrupts. The judged rule admits text.
    let probe = test(
        &layout,
        &project,
        &config,
        "main",
        &Sample::Text("TODO: rm -rf build".into()),
    )
    .unwrap();
    let fired: Vec<(String, &str)> = probe
        .matches
        .iter()
        .map(|m| (source(m), m.trigger.as_str()))
        .collect();
    assert!(
        fired.contains(&("stop.md".into(), "interrupt")),
        "{fired:?}"
    );
    assert!(
        fired.contains(&("remind.md".into(), "deferred")),
        "{fired:?}"
    );
    assert_eq!(fired.len(), 2, "{fired:?}");
    assert_eq!(probe.judged.len(), 1);
    // A tool call reaches only tool-scoped rules; `remind` and the judged rule are text-only.
    let probe = test(
        &layout,
        &project,
        &config,
        "main",
        &Sample::Tool(ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":"TODO; rm -rf /tmp/x"}),
            malformed_arguments: false,
        }),
    )
    .unwrap();
    assert_eq!(probe.matches.len(), 1, "{probe:?}");
    assert_eq!(source(&probe.matches[0]), "stop.md");
    assert!(probe.judged.is_empty());
}

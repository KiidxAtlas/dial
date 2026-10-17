use super::*;

fn known() -> Vec<String> {
    known_tool_names(&[])
}

#[test]
fn split_csv_trims_and_drops_empty() {
    assert_eq!(split_csv(" read , ,bash,"), vec!["read", "bash"]);
    assert!(split_csv(" , ").is_empty());
}

#[test]
fn normalize_dedupes_and_lowercases_only_known_names() {
    let names = normalize_tool_names("READ, read, MyMcpTool, Bash", &known());
    assert_eq!(names, vec!["read", "MyMcpTool", "bash"]);
}

#[test]
fn validate_reports_all_unknown_names() {
    let requested = vec!["read".to_owned(), "nope".to_owned(), "zap".to_owned()];
    let err = validate_tool_names(&requested, &known())
        .unwrap_err()
        .to_string();
    assert!(err.contains("nope") && err.contains("zap"), "{err}");
    assert!(!err.contains("read"), "{err}");
}

#[test]
fn integrations_for_tools_selects_only_gated_names() {
    let tools = vec!["read".to_owned(), "lsp".to_owned(), "web_fetch".to_owned()];
    assert_eq!(integrations_for_tools(&tools), vec!["lsp", "web_fetch"]);
}

#[test]
fn resolve_prompt_input_treats_newline_as_literal() {
    assert_eq!(
        resolve_prompt_input("line one\nline two"),
        "line one\nline two"
    );
}

#[test]
fn resolve_prompt_input_reads_file_and_falls_back_to_literal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prompt.md");
    std::fs::write(&path, "from file").unwrap();
    assert_eq!(resolve_prompt_input(path.to_str().unwrap()), "from file");
    assert_eq!(resolve_prompt_input("just words"), "just words");
}

#[test]
fn prompt_flags_are_mutually_exclusive() {
    let args = ToolFlagArgs {
        system_prompt: Some("x".into()),
        system_prompt_template: Some("t".into()),
        ..Default::default()
    };
    assert!(validate_prompt_flags(&args).is_err());
    let mut overlay = toml::Table::new();
    assert!(apply(&args, &mut overlay, &[], &[]).is_err());
}

#[test]
fn tools_flag_sets_harness_allowlist_and_integrations() {
    let args = ToolFlagArgs {
        tools: Some("read,lsp".into()),
        no_tools: true,
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let out = apply(&args, &mut overlay, &["web_search".into()], &[]).unwrap();
    assert_eq!(out.tools, Some(vec!["read".to_owned(), "lsp".to_owned()]));
    let harness = overlay["harness"].as_table().unwrap();
    assert_eq!(harness["tools"].as_array().unwrap().len(), 2);
    assert_eq!(overlay["integrations"].as_array().unwrap().len(), 1);
    assert!(out.partial.is_empty(), "--tools is a hard allowlist now");
}

#[test]
fn no_tools_alone_empties_allowlist() {
    let args = ToolFlagArgs {
        no_tools: true,
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let out = apply(&args, &mut overlay, &["lsp".into()], &[]).unwrap();
    assert_eq!(out.tools, Some(Vec::new()));
    assert!(overlay["integrations"].as_array().unwrap().is_empty());
}

#[test]
fn no_lsp_removes_only_lsp_integration() {
    let args = ToolFlagArgs {
        no_lsp: true,
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let base = vec!["lsp".to_owned(), "web_search".to_owned()];
    apply(&args, &mut overlay, &base, &[]).unwrap();
    let got: Vec<&str> = overlay["integrations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(got, vec!["web_search"]);
}

#[test]
fn no_rules_disables_portable_rules() {
    let args = ToolFlagArgs {
        no_rules: true,
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    apply(&args, &mut overlay, &[], &[]).unwrap();
    assert_eq!(overlay["portable_rules"]["enabled"].as_bool(), Some(false));
}

#[test]
fn no_skills_wins_over_skills() {
    let args = ToolFlagArgs {
        no_skills: true,
        skills: Some("a,b".into()),
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let out = apply(&args, &mut overlay, &[], &[]).unwrap();
    assert_eq!(out.skills, Some(Vec::new()));
    assert!(out.blocked.is_empty());
}

#[test]
fn prompt_text_and_skill_list_are_wired() {
    let args = ToolFlagArgs {
        system_prompt: Some("you are terse".into()),
        skills: Some("review".into()),
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let out = apply(&args, &mut overlay, &[], &[]).unwrap();
    assert_eq!(out.system_prompt.as_deref(), Some("you are terse"));
    assert_eq!(out.skills, Some(vec!["review".to_owned()]));
    assert!(out.blocked.is_empty());
}

#[test]
fn no_pty_flag_writes_shell_overlay() {
    let args = ToolFlagArgs {
        no_pty: true,
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    apply(&args, &mut overlay, &[], &[]).unwrap();
    assert_eq!(overlay["shell"]["no_pty"].as_bool(), Some(true));
}
#[test]
fn unapplied_flags_are_reported_not_dropped() {
    let args = ToolFlagArgs {
        add_dir: vec![PathBuf::from("/tmp/x")],
        no_title: true,
        append_system_prompt: Some("more".into()),
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let out = apply(&args, &mut overlay, &[], &[]).unwrap();
    let flags: Vec<&str> = out.blocked.iter().map(|b| b.flag).collect();
    assert!(!flags.contains(&"--add-dir"));
    assert!(!flags.contains(&"--append-system-prompt"));
    assert_eq!(out.notes[0].flag, "--no-title");
    assert_eq!(out.add_dirs, vec![PathBuf::from("/tmp/x")]);
}

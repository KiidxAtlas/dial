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
fn kernel_alias_resolves_to_eval_in_tools_and_integrations() {
    assert_eq!(canonical_tool_name("kernel"), "eval");
    assert_eq!(canonical_tool_name("Kernel"), "eval");
    assert_eq!(canonical_tool_name("read"), "read");
    assert_eq!(
        normalize_tool_names("kernel, read, KERNEL", &known()),
        vec!["eval", "read"]
    );
    let tools = vec!["kernel".to_owned(), "read".to_owned()];
    assert_eq!(integrations_for_tools(&tools), vec!["eval"]);
    let args = ToolFlagArgs {
        tools: Some("kernel".into()),
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let out = apply(&args, &mut overlay, &[], &[]).unwrap();
    assert_eq!(out.tools, Some(vec!["eval".to_owned()]));
    assert_eq!(overlay["harness"]["tools"][0].as_str(), Some("eval"));
    assert_eq!(overlay["integrations"][0].as_str(), Some("eval"));
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
fn configured_custom_tools_are_accepted_without_executing_the_program() {
    let config: crate::config::types::Config = toml::from_str(
        r#"
        [tools.LocalCheck]
        program = "/not/a/real/program"
        "#,
    )
    .unwrap();
    let extra: Vec<String> = config.tools.keys().cloned().collect();
    let args = ToolFlagArgs {
        tools: Some("READ,LocalCheck,LocalCheck".into()),
        ..Default::default()
    };
    let mut overlay = toml::Table::new();
    let out = apply(&args, &mut overlay, &config.integrations, &extra).unwrap();
    assert_eq!(out.tools, Some(vec!["read".into(), "LocalCheck".into()]));
    assert_eq!(overlay["harness"]["tools"][1].as_str(), Some("LocalCheck"));

    let invalid = ToolFlagArgs {
        tools: Some("LocalCheck,localcheck,not_configured".into()),
        ..Default::default()
    };
    let mut untouched = toml::Table::new();
    let error = apply(&invalid, &mut untouched, &config.integrations, &extra)
        .unwrap_err()
        .to_string();
    assert!(error.contains("localcheck, not_configured"), "{error}");
    assert!(untouched.is_empty());
}

#[test]
fn dynamic_tools_require_exact_discovered_names_not_server_prefixes() {
    let discovered = vec!["mcp__demo__FetchItem".into(), "extension_lookup".into()];
    let args = ToolFlagArgs {
        tools: Some("mcp__demo__FetchItem,extension_lookup".into()),
        ..Default::default()
    };
    let out = apply(&args, &mut toml::Table::new(), &[], &discovered).unwrap();
    assert_eq!(out.tools, Some(discovered.clone()));

    for name in [
        "mcp__demo__invented",
        "mcp__other__FetchItem",
        "extension_invented",
    ] {
        let invalid = ToolFlagArgs {
            tools: Some(name.into()),
            ..Default::default()
        };
        assert!(apply(&invalid, &mut toml::Table::new(), &[], &discovered).is_err());
    }
    // A syntactically plausible name without discovery is not proof of registration.
    assert!(apply(&args, &mut toml::Table::new(), &[], &[]).is_err());
}

#[test]
fn launch_defers_only_api_valid_unknown_identifiers() {
    let args = ToolFlagArgs {
        tools: Some("READ,mcp__demo__FetchItem,extension_lookup".into()),
        ..Default::default()
    };
    let out = apply_for_launch(&args, &mut toml::Table::new(), &[], &[]).unwrap();
    assert_eq!(
        out.deferred_tools,
        vec!["mcp__demo__FetchItem", "extension_lookup"]
    );
    assert_eq!(out.tools.unwrap()[0], "read");
    for name in ["bad.name".to_owned(), "x".repeat(65)] {
        let invalid = ToolFlagArgs {
            tools: Some(name),
            ..Default::default()
        };
        let mut overlay = toml::Table::new();
        assert!(apply_for_launch(&invalid, &mut overlay, &[], &[]).is_err());
        assert!(overlay.is_empty());
    }
    let disabled = ToolFlagArgs {
        no_tools: true,
        ..Default::default()
    };
    let out = apply_for_launch(&disabled, &mut toml::Table::new(), &[], &[]).unwrap();
    assert_eq!(out.tools, Some(vec![]));
    assert!(out.deferred_tools.is_empty());
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

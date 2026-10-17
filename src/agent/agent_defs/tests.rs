use crate::config::task::TaskConfig;

use super::*;

#[test]
fn custom_names_and_disabled_thinking_work_in_toml_and_markdown() {
    for level in ["off", "none"] {
        let config: Config = toml::from_str(&format!(
            "[agents.custom-review]\nbase='reviewer'\nprompt='Review code.'\nthinking_level='{level}'\n"
        ))
        .unwrap();
        config.validate().unwrap();
        let catalog = Catalog::load(&config);
        assert_eq!(
            catalog.get("custom-review").unwrap().thinking.as_deref(),
            Some(level)
        );
        let markdown = parse_definition(
            &format!("---\nname: custom-review\ndescription: reviews code\nthinking_level: {level}\n---\nReview code."),
            "custom-review",
            Source::Project,
        )
        .unwrap();
        assert_eq!(markdown.thinking.as_deref(), Some(level));
    }
}

#[test]
fn frontmatter_fields_parse_and_writers_are_inferred() {
    let text = "---\nname: migrator\ndescription: applies migrations\ntools: [read, grep, edit, write, task]\nmodel: [\"@small\", openai/gpt-5]\nthinking_level: low\nmax_steps: 7\nautoload_skills: db, sql\noutput:\n  type: object\n  required: [files]\n  properties:\n    files: { type: array, items: { type: string } }\n---\nYou migrate schemas.\n";
    let def = parse_definition(text, "file", Source::Project).unwrap();
    assert_eq!(def.name, "migrator");
    assert!(def.mutates, "edit/write make a writer");
    assert_eq!(def.spawns, Spawns::All, "listing task implies spawning");
    assert_eq!(
        def.restrict.as_deref(),
        Some(
            &[
                "read".to_owned(),
                "grep".to_owned(),
                "edit".to_owned(),
                "write".to_owned()
            ][..]
        )
    );
    assert_eq!(def.model, ["@small", "openai/gpt-5"]);
    assert_eq!(def.thinking.as_deref(), Some("low"));
    assert_eq!(def.max_steps, 7);
    assert_eq!(def.autoload_skills, ["db", "sql"]);
    assert_eq!(def.output.as_ref().unwrap()["required"][0], "files");
    assert_eq!(def.prompt, "You migrate schemas.");
}

#[test]
fn read_only_by_default_with_explicit_spawn_list_and_name_from_stem() {
    let def = parse_definition(
        "---\ndescription: d\nspawns: scout, reviewer\n---\nbody",
        "lead",
        Source::User,
    )
    .unwrap();
    assert_eq!(def.name, "lead");
    assert!(!def.mutates && def.restrict.is_none());
    assert!(def.spawns.allows("scout") && !def.spawns.allows("worker"));
}

#[test]
fn claude_definitions_map_tool_names_and_drop_model_aliases() {
    let def = parse_definition(
        "---\nname: fixer\ndescription: fixes\ntools: Read, Grep, Edit, Bash\nmodel: sonnet\ncolor: red\n---\nFix it.",
        "fixer",
        Source::ClaudeProject,
    )
    .unwrap();
    assert_eq!(def.restrict.unwrap(), ["read", "grep", "edit", "bash"]);
    assert!(def.mutates && def.model.is_empty());
}

#[test]
fn invalid_definitions_are_refused_with_a_reason() {
    let bad = |text: &str| parse_definition(text, "x", Source::Project).unwrap_err();
    assert!(bad("---\nname: scout\ndescription: d\n---\nb").contains("reserved"));
    assert!(bad("---\nname: x\n---\nb").contains("description"));
    assert!(bad("---\ndescription: d\n---\n   ").contains("empty"));
    assert!(bad("---\ndescription: d\nthinking_level: galaxy\n---\nb").contains("thinking"));
    assert!(bad("---\ndescription: d\nb").contains("not closed"));
    assert!(bad("---\nname: main\ndescription: d\n---\nb").contains("invalid agent name"));
}

#[test]
fn spawn_depth_follows_the_recursion_limit() {
    let mut task = TaskConfig::default();
    assert!(task.can_spawn_at(0) && task.can_spawn_at(1) && !task.can_spawn_at(2));
    task.max_recursion_depth = 0;
    assert!(!task.can_spawn_at(0));
    task.max_recursion_depth = -1;
    assert!(task.can_spawn_at(40));
    task.validate().unwrap();
}

#[test]
fn catalog_loads_project_files_and_writers_need_approval() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap();
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(project.join(".dial/agents")).unwrap();
    std::fs::write(
        project.join(".dial/agents/porter.md"),
        "---\ndescription: ports code\ntools: [read, edit]\n---\nPort it.",
    )
    .unwrap();
    std::fs::write(
        project.join(".dial/agents/looker.md"),
        "---\ndescription: looks\n---\nLook.",
    )
    .unwrap();
    let config = Config {
        project_root: project,
        ..Config::default()
    };
    let catalog = Catalog::load(&config);
    assert!(
        catalog
            .get("porter")
            .is_some_and(|a| a.mutates && a.source == Source::Project)
    );
    assert!(catalog.get("looker").is_some_and(|a| !a.mutates));
    assert!(launches_writer("porter") && !launches_writer("looker") && !launches_writer("scout"));
    assert!(
        launches_writer("never-defined-anywhere"),
        "unknown agents fail closed"
    );
}

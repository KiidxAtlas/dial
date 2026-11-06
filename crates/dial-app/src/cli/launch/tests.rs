use super::*;

#[test]
fn unset_flags_leave_launch_unrestricted() {
    let launch = agent_launch(&EffectiveSettings::default(), &ModelFlags::default());
    assert!(launch.tools.is_none());
    assert!(launch.skills.is_none());
    assert!(launch.deadline.is_none());
    assert!(launch.system_prompt.is_none());
    assert!(launch.append_system_prompt.is_none());
    assert!(launch.allows_tool("bash"));
}

#[test]
fn tools_and_skills_map_to_allowlists() {
    let settings = EffectiveSettings {
        tools: Some(vec!["read".into(), "read".into(), "bash".into()]),
        skills: Some(vec!["review".into()]),
        ..Default::default()
    };
    let launch = agent_launch(&settings, &ModelFlags::default());
    assert_eq!(launch.tools.as_ref().map(BTreeSet::len), Some(2));
    assert!(launch.allows_tool("bash"));
    assert!(!launch.allows_tool("write"));
    assert!(launch.skill_visible("review"));
    assert!(!launch.skill_visible("deploy"));
}

#[test]
fn candidate_names_carry_through_to_agent_validation() {
    let settings = crate::cli::tool_flags::apply_for_launch(
        &crate::cli::tool_flags::ToolFlagArgs {
            tools: Some("mcp__demo__FetchItem".into()),
            ..Default::default()
        },
        &mut toml::Table::new(),
        &[],
        &[],
    )
    .unwrap();
    let launch = agent_launch(&settings, &ModelFlags::default());
    assert_eq!(
        launch.deferred_tools,
        ["mcp__demo__FetchItem".into()].into()
    );
    assert_eq!(launch.tools.unwrap(), launch.deferred_tools);
}

#[test]
fn no_tools_and_no_skills_withhold_everything_optional() {
    let settings = EffectiveSettings {
        tools: Some(Vec::new()),
        skills: Some(Vec::new()),
        ..Default::default()
    };
    let launch = agent_launch(&settings, &ModelFlags::default());
    assert!(!launch.allows_tool("bash"));
    assert!(launch.allows_tool("finish"));
    assert!(!launch.allows_tool("skill"));
}

#[test]
fn prompt_text_and_deadline_carry_through() {
    let settings = EffectiveSettings {
        system_prompt: Some("terse".into()),
        append_system_prompt: Some("cite sources".into()),
        ..Default::default()
    };
    let model = ModelFlags {
        max_time: Some(30.0),
        ..Default::default()
    };
    let launch = agent_launch(&settings, &model);
    assert_eq!(launch.system_prompt.as_deref(), Some("terse"));
    assert_eq!(launch.append_system_prompt.as_deref(), Some("cite sources"));
    assert!(launch.deadline.is_some());
}

#[test]
fn read_only_directory_flags_are_repeatable_and_launch_local() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        tools: crate::cli::tool_flags::ToolFlagArgs,
    }
    let cli = Cli::try_parse_from([
        "dial",
        "--read-only-dir",
        "/reference",
        "--read-only-dir",
        "/fixtures",
    ])
    .unwrap();
    let mut overlay = toml::Table::new();
    let settings = crate::cli::tool_flags::apply(&cli.tools, &mut overlay, &[], &[]).unwrap();
    let launch = agent_launch(&settings, &ModelFlags::default());
    assert_eq!(
        launch.read_only_dirs,
        [
            std::path::PathBuf::from("/reference"),
            std::path::PathBuf::from("/fixtures")
        ]
    );
    assert!(settings.add_dirs.is_empty());
    assert!(overlay.is_empty());
    assert!(
        agent_launch(&EffectiveSettings::default(), &ModelFlags::default())
            .read_only_dirs
            .is_empty()
    );
}

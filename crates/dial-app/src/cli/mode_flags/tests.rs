use super::*;
use clap::Parser;

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    mode: ModeArgs,
}

fn parse(args: &[&str]) -> ModeArgs {
    let mut argv = vec!["dial"];
    argv.extend_from_slice(args);
    TestCli::try_parse_from(argv).unwrap().mode
}

fn interactive() -> LaunchContext {
    LaunchContext {
        interactive: true,
        ..Default::default()
    }
}

#[test]
fn protocol_launch_rejects_agent_modes_instead_of_ignoring_intent() {
    for args in [
        vec!["--plan"],
        vec!["--plan", "planner"],
        vec!["--plan-yolo"],
        vec!["--prewalk"],
        vec!["--advisor"],
        vec!["--external-thinking"],
    ] {
        let flags = resolve(&parse(&args), interactive()).unwrap();
        for protocol in ["RPC", "ACP"] {
            let error = flags.validate_protocol(protocol).unwrap_err().to_string();
            assert!(error.contains(args[0]), "{error}");
            assert!(error.contains(protocol), "{error}");
        }
    }
    let flags = resolve(
        &parse(&["--hide-thinking", "--no-extensions"]),
        interactive(),
    )
    .unwrap();
    assert!(flags.validate_protocol("RPC").is_ok());
}

#[test]
fn goal_objective_is_trimmed_and_required() {
    assert_eq!(normalize_goal("  ship it  ").unwrap(), "ship it");
    let err = normalize_goal("   ").unwrap_err().to_string();
    assert_eq!(err, "--goal requires a non-empty objective.");
}

#[test]
fn goal_launch_rejects_non_interactive_and_resume() {
    let print = LaunchContext {
        interactive: false,
        ..Default::default()
    };
    assert!(validate_goal_launch(Some("x"), print).is_err());
    let resumed = LaunchContext {
        interactive: true,
        resumes_session: true,
        ..Default::default()
    };
    assert!(validate_goal_launch(Some("x"), resumed).is_err());
    assert!(validate_goal_launch(Some("x"), interactive()).is_ok());
    assert!(validate_goal_launch(None, LaunchContext::default()).is_ok());
}

#[test]
fn goal_startup_rejects_conflicting_inputs() {
    let ok_base = GoalStartup::default();
    assert!(validate_goal_startup(Some("x"), ok_base).is_ok());
    let prompt = GoalStartup {
        has_positional_prompt: true,
        ..ok_base
    };
    assert!(
        validate_goal_startup(Some("x"), prompt)
            .unwrap_err()
            .to_string()
            .contains("positional")
    );
    let yolo = GoalStartup {
        plan_yolo: true,
        ..ok_base
    };
    assert!(
        validate_goal_startup(Some("x"), yolo)
            .unwrap_err()
            .to_string()
            .contains("--plan-yolo")
    );
    let plan = GoalStartup {
        plan_starts_on_startup: true,
        ..ok_base
    };
    assert!(
        validate_goal_startup(Some("x"), plan)
            .unwrap_err()
            .to_string()
            .contains("plan mode")
    );
}

#[test]
fn trusted_extensions_must_be_absolute_and_exclusive() {
    let abs = vec!["/opt/ext.js".to_string()];
    assert_eq!(
        validate_trusted_extensions(&abs, false).unwrap(),
        vec![PathBuf::from("/opt/ext.js")]
    );
    let rel = vec!["ext.js".to_string()];
    assert!(
        validate_trusted_extensions(&rel, false)
            .unwrap_err()
            .to_string()
            .contains("absolute")
    );
    assert!(validate_trusted_extensions(&[" ".to_string()], false).is_err());
    assert!(
        validate_trusted_extensions(&abs, true)
            .unwrap_err()
            .to_string()
            .contains("cannot be combined")
    );
    assert!(validate_trusted_extensions(&[], true).unwrap().is_empty());
}

#[test]
fn overlay_sets_only_requested_keys() {
    assert!(config_overlay(false, false).is_empty());
    let o = config_overlay(true, true);
    assert_eq!(o["ui"]["show_thinking"].as_bool(), Some(false));
    assert_eq!(o["extensions"]["enabled"].as_bool(), Some(false));
    let mut base = toml::Table::new();
    crate::config::merge(&mut base, config_overlay(true, false));
    assert_eq!(base["ui"]["show_thinking"].as_bool(), Some(false));
    assert!(base.get("extensions").is_none());
}

#[test]
fn parses_bare_and_named_plan_and_repeatable_flags() {
    let bare = parse(&["--plan"]);
    assert_eq!(bare.plan.as_deref(), Some(""));
    let named = parse(&["--plan", "smart"]);
    assert_eq!(named.plan.as_deref(), Some("smart"));
    let ext = parse(&["-e", "a.js", "--extension", "b.js"]);
    assert_eq!(ext.extensions, vec!["a.js", "b.js"]);
}

#[test]
fn plan_flag_enables_plan_mode() {
    let flags = resolve(&parse(&["--plan"]), interactive()).unwrap();
    assert!(flags.plan_mode);
    assert_eq!(flags.plan_model, None);
    assert!(!resolve(&parse(&[]), interactive()).unwrap().plan_mode);
}

#[test]
fn plan_model_resolves_aliases_into_the_planner_role() {
    let mut config = crate::config::types::Config::default();
    config.model_roles.insert(
        "small".to_owned(),
        crate::llm::model_config::ModelSelection {
            provider: "openai".to_owned(),
            model: "gpt-small".to_owned(),
            ..Default::default()
        },
    );
    let aliased = resolve(&parse(&["--plan", "@small"]), interactive()).unwrap();
    assert!(aliased.plan_mode);
    assert_eq!(aliased.plan_model.as_deref(), Some("@small"));
    let applied = aliased.apply_plan_model(config.clone()).unwrap();
    assert_eq!(applied.model_roles["planner"].model, "gpt-small");
    let bare = resolve(&parse(&["--plan"]), interactive()).unwrap();
    assert_eq!(
        bare.apply_plan_model(config.clone())
            .unwrap()
            .model_roles
            .get("planner"),
        None
    );
    let err = resolve(&parse(&["--plan", "no-such-model"]), interactive())
        .unwrap()
        .apply_plan_model(config)
        .unwrap_err()
        .to_string();
    assert_eq!(
        err,
        "Model \"no-such-model\" not found. Run \"dial models\" to see available models."
    );
}

#[test]
fn honored_flags_produce_overlay() {
    let flags = resolve(
        &parse(&["--hide-thinking", "--no-extensions"]),
        interactive(),
    )
    .unwrap();
    assert_eq!(flags.overlay["ui"]["show_thinking"].as_bool(), Some(false));
    assert_eq!(
        flags.overlay["extensions"]["enabled"].as_bool(),
        Some(false)
    );
    let mut target = toml::Table::new();
    flags.merge_into_overlay(&mut target);
    assert_eq!(target, flags.overlay);
}

#[test]
fn no_prewalk_is_accepted_as_existing_behavior() {
    assert!(resolve(&parse(&["--no-prewalk"]), interactive()).is_ok());
}

#[test]
fn prewalk_defaults_to_small_and_into_names_the_target() {
    assert_eq!(
        resolve(&parse(&["--prewalk"]), interactive())
            .unwrap()
            .prewalk
            .as_deref(),
        Some("@small")
    );
    let into = parse(&["--prewalk", "--prewalk-into", "@main"]);
    assert_eq!(
        resolve(&into, interactive()).unwrap().prewalk.as_deref(),
        Some("@main")
    );
    assert_eq!(resolve(&parse(&[]), interactive()).unwrap().prewalk, None);
}

#[test]
fn goal_flag_resolves_to_trimmed_objective() {
    let flags = resolve(&parse(&["--goal", "  ship it  "]), interactive()).unwrap();
    assert_eq!(flags.goal.as_deref(), Some("ship it"));
    assert!(!flags.plan_mode);
    assert_eq!(flags.plan_yolo, None);
}

#[test]
fn goal_flag_rejects_positional_prompt_and_plan_yolo() {
    let positional = LaunchContext {
        has_positional_prompt: true,
        ..interactive()
    };
    assert!(resolve(&parse(&["--goal", "ship"]), positional).is_err());
    assert!(resolve(&parse(&["--goal", "ship", "--plan-yolo"]), interactive()).is_err());
}

#[test]
fn plan_yolo_resolves_target_and_plan_mode() {
    let bare = resolve(&parse(&["--plan-yolo"]), interactive()).unwrap();
    assert!(bare.plan_mode);
    assert_eq!(bare.plan_yolo.as_deref(), Some("@small"));
    let into = resolve(
        &parse(&["--plan-yolo", "--plan-yolo-into", "@main"]),
        interactive(),
    )
    .unwrap();
    assert_eq!(into.plan_yolo.as_deref(), Some("@main"));
    let plain = resolve(&parse(&["--plan"]), interactive()).unwrap();
    assert_eq!(plain.plan_yolo, None);
}

#[test]
fn advisor_and_external_thinking_resolve_launch_scoped() {
    let flags = resolve(&parse(&["--advisor", "--external-thinking"]), interactive()).unwrap();
    assert!(flags.advisor);
    assert!(flags.external_thinking);
    assert!(
        flags.overlay.is_empty(),
        "launch-scoped flags never reach the config overlay"
    );
    let plain = resolve(&parse(&[]), interactive()).unwrap();
    assert!(!plain.advisor && !plain.external_thinking);
}

#[test]
fn plan_yolo_into_requires_plan_yolo() {
    let mut argv = vec!["dial", "--plan-yolo-into", "m"];
    assert!(TestCli::try_parse_from(argv.clone()).is_err());
    argv.push("--plan-yolo");
    assert!(TestCli::try_parse_from(argv).is_ok());
}

#[test]
fn prewalk_conflicts_with_no_prewalk() {
    assert!(TestCli::try_parse_from(["dial", "--prewalk", "--no-prewalk"]).is_err());
}

fn temp_module_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dial-mode-flags-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn extension_flag_lands_in_plugins_overlay() {
    let dir = temp_module_dir("extension");
    let module = dir.join("ext.js");
    std::fs::write(&module, "").unwrap();
    let flags = resolve(&parse(&["-e", module.to_str().unwrap()]), interactive()).unwrap();
    let program = flags.overlay["extensions"]["plugins"]["cli-0-ext.js"]["program"]
        .as_str()
        .unwrap();
    assert_eq!(
        program,
        std::fs::canonicalize(&module).unwrap().to_str().unwrap()
    );
    assert!(flags.overlay["extensions"].get("discover").is_none());
    let missing = resolve(&parse(&["-e", "/no/such/module.js"]), interactive())
        .unwrap_err()
        .to_string();
    assert!(missing.contains("not found"), "{missing}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn trusted_extension_flag_is_an_exact_allowlist_without_discovery() {
    let dir = temp_module_dir("trusted");
    let module = dir.join("policy.js");
    std::fs::write(&module, "").unwrap();
    let flags = resolve(
        &parse(&["--trusted-extension", module.to_str().unwrap()]),
        interactive(),
    )
    .unwrap();
    assert_eq!(
        flags.overlay["extensions"]["discover"].as_bool(),
        Some(false)
    );
    assert!(
        flags.overlay["extensions"]["plugins"]
            .as_table()
            .unwrap()
            .contains_key("trusted-0-policy.js")
    );
    assert!(resolve(&parse(&["--trusted-extension", "rel.js"]), interactive()).is_err());
    let mixed = [
        "--trusted-extension",
        module.to_str().unwrap(),
        "-e",
        module.to_str().unwrap(),
    ];
    assert!(
        resolve(&parse(&mixed), interactive())
            .unwrap_err()
            .to_string()
            .contains("cannot be combined")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn hook_flag_lands_in_hooks_overlay_for_every_event() {
    let dir = temp_module_dir("hook");
    let module = dir.join("hook.sh");
    std::fs::write(&module, "").unwrap();
    let flags = resolve(&parse(&["--hook", module.to_str().unwrap()]), interactive()).unwrap();
    let hooks = flags.overlay["hooks"].as_array().unwrap();
    assert_eq!(hooks.len(), crate::config::hooks::EVENTS.len());
    let program = std::fs::canonicalize(&module)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for (hook, event) in hooks.iter().zip(crate::config::hooks::EVENTS) {
        assert_eq!(hook["event"].as_str(), Some(*event));
        assert_eq!(hook["command"][0].as_str(), Some(program.as_str()));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn hook_overlay_appends_to_configured_hooks_instead_of_replacing_them() {
    let dir = temp_module_dir("hook-append");
    let module = dir.join("hook.sh");
    std::fs::write(&module, "").unwrap();
    let flags = resolve(&parse(&["--hook", module.to_str().unwrap()]), interactive()).unwrap();
    let project_hook = toml::Value::Table(
        toml::from_str("event = 'pre_tool_use'\ncommand = ['project-policy']").unwrap(),
    );
    let mut target = toml::Table::new();
    target.insert(
        "hooks".into(),
        toml::Value::Array(vec![project_hook.clone()]),
    );
    flags.merge_into_overlay(&mut target);
    let hooks = target["hooks"].as_array().unwrap();
    assert_eq!(hooks[0], project_hook, "the configured hook is kept first");
    assert_eq!(hooks.len(), 1 + crate::config::hooks::EVENTS.len());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn plugin_dir_flag_lands_every_plugin_program_in_overlay() {
    let dir = temp_module_dir("plugin-dir");
    std::fs::write(dir.join("a.js"), "").unwrap();
    std::fs::write(dir.join("b.sh"), "").unwrap();
    let flags = resolve(
        &parse(&["--plugin-dir", dir.to_str().unwrap()]),
        interactive(),
    )
    .unwrap();
    let plugins = flags.overlay["extensions"]["plugins"].as_table().unwrap();
    assert_eq!(plugins.len(), 2);
    assert!(plugins.contains_key("dir-0-a.js") && plugins.contains_key("dir-0-b.sh"));
    let empty = temp_module_dir("plugin-dir-empty");
    let err = resolve(
        &parse(&["--plugin-dir", empty.to_str().unwrap()]),
        interactive(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("no plugin programs"), "{err}");
    assert!(resolve(&parse(&["--plugin-dir", "/no/such/dir"]), interactive()).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_dir_all(&empty).unwrap();
}

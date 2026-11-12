use super::*;

#[test]
fn notebook_is_registered_only_when_listed_as_integration() {
    let mut config = Config {
        integrations: vec!["notebook".into()],
        ..Config::default()
    };
    assert!(Registry::standard(&config, Vec::new()).contains("notebook"));

    config.integrations.clear();
    assert!(!Registry::standard(&config, Vec::new()).contains("notebook"));
}
#[test]
fn legacy_integration_lists_keep_always_registered_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: Vec::new(),
        ..Config::default()
    };
    // `image_search` is always registered, but only with an explicit feature model.
    config.vision.image_embedding = Some(crate::llm::model_config::ModelSelection::new(
        "mlx",
        "test/embedder",
    ));
    let registry = Registry::standard(&config, Vec::new());
    for entry in crate::config::tools::integrations::INTEGRATIONS {
        if entry.always_registered {
            assert!(registry.contains(entry.name), "{}", entry.name);
        }
    }
    assert!(!registry.contains("computer"));
    assert!(!registry.contains("notebook"));
}

#[test]
fn video_registry_requires_opt_in_and_integration_but_investigation_tools_are_discoverable() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: vec!["video".into()],
        ..Config::default()
    };
    let registry = Registry::standard(&config, Vec::new());
    assert!(!registry.contains("video"));
    for name in ["document", "audio", "data", "evidence"] {
        assert!(registry.contains(name), "{name}");
    }
    for retired in [
        "investigate",
        "workbench",
        "evidence_search",
        "reality",
        "timeline",
    ] {
        assert!(!registry.contains(retired), "{retired}");
    }
    config.experimental.video_analysis = true;
    assert!(Registry::standard(&config, Vec::new()).contains("video"));
    config.integrations.clear();
    assert!(!Registry::standard(&config, Vec::new()).contains("video"));
}

#[test]
fn system_investigation_tools_are_read_only_discoverable_and_have_valid_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: Vec::new(),
        ..Config::default()
    };
    let registry = Registry::standard(&config, Vec::new());
    for name in ["runtime_inspect", "network_inspect", "database"] {
        let tool = registry
            .get(name)
            .expect("system investigation tool registered");
        assert!(tool.read_only_hint(), "{name}");
        assert!(!tool.needs_approval(&serde_json::json!({})), "{name}");
        assert!(!tool.may_modify_workspace(&serde_json::json!({})), "{name}");
        let schema = tool.schema();
        assert_eq!(
            crate::tools::surface::catalog::validate_definition(&schema).unwrap(),
            name
        );
        assert!(
            crate::tools::surface::catalog::validate_arguments(
                &schema,
                &serde_json::json!({"action":"invented", "path":"fixture"})
            )
            .is_err(),
            "{name} accepted invalid action"
        );
    }
}

#[test]
fn consolidated_members_are_reached_through_one_valid_successor() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        integrations: Vec::new(),
        ..Config::default()
    };
    let registry = Registry::standard(&config, Vec::new());
    let names: Vec<&str> = registry.names().collect();
    for member in [
        "document",
        "data",
        "database",
        "runtime_inspect",
        "network_inspect",
    ] {
        assert!(!names.contains(&member), "{member} still listed: {names:?}");
        assert_eq!(registry.get(member).unwrap().name(), member);
    }
    assert!(names.contains(&"inspect"), "{names:?}");
    let inspect = registry.get("inspect").unwrap();
    let schema = inspect.schema();
    assert_eq!(
        crate::tools::surface::catalog::validate_definition(&schema).unwrap(),
        "inspect"
    );
    let kinds = &schema["function"]["parameters"]["properties"]["kind"]["enum"];
    assert!(
        kinds
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("har")),
        "{kinds}"
    );
    assert!(!inspect.needs_approval(&serde_json::json!({"kind":"trace","action":"summary"})));

    let forked = registry.fork_session();
    assert_eq!(forked.names().collect::<Vec<_>>(), names);

    let mut registry = registry;
    for member in [
        "document",
        "data",
        "database",
        "visual",
        "audio",
        "video",
        "runtime_inspect",
        "network_inspect",
    ] {
        registry.remove(member);
    }
    assert!(!registry.contains("inspect"));
}

#[test]
fn every_consolidation_group_builds_without_shadowed_routes() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        project_root: dir.path().to_path_buf(),
        user_dir: dir.path().join("user"),
        state_dir: dir.path().join("state"),
        ..Config::default()
    };
    config.experimental.video_analysis = true;
    for group in crate::tools::surface::facade::GROUPS {
        config.integrations.push(group.successor.into());
        config
            .integrations
            .extend(group.members.iter().map(|(name, _)| (*name).to_owned()));
    }
    let registry = Registry::standard(&config, Vec::new());
    let mut built = Vec::new();
    for group in crate::tools::surface::facade::GROUPS {
        let native = registry.owner(group.successor).filter(|tool| {
            group
                .members
                .iter()
                .all(|(member, _)| *member != group.successor)
                && tool.name() == group.successor
                && registry.folded.contains_key(group.successor)
        });
        let present: Vec<(&'static str, Arc<dyn Tool>)> = group
            .members
            .iter()
            .filter_map(|(name, _)| {
                registry
                    .folded
                    .get(*name)
                    .map(|tool| (*name, Arc::clone(tool)))
            })
            .collect();
        let Some(facade) = crate::tools::surface::facade::build(group, native.cloned(), &present)
        else {
            continue;
        };
        built.push(group.successor);
        assert!(
            facade.collisions().is_empty(),
            "{}: {:?}",
            group.successor,
            facade.collisions()
        );
        let schema = facade.schema();
        crate::tools::surface::catalog::validate_definition(&schema).unwrap();
        for (name, tool) in &present {
            let actions =
                tool.schema()["function"]["parameters"]["properties"]["action"]["enum"].clone();
            for action in actions.as_array().into_iter().flatten() {
                let recorded = serde_json::json!({ "action": action });
                let Some((successor, call)) =
                    crate::tools::surface::facade::successor_call(name, &recorded)
                else {
                    panic!("{name} {recorded} has no projection");
                };
                assert_eq!(successor, group.successor);
                let properties = &schema["function"]["parameters"]["properties"];
                for key in call.as_object().unwrap().keys() {
                    assert!(properties.get(key).is_some(), "{name}: {key} not offered");
                }
                assert!(
                    facade
                        .resolve(&call)
                        .is_some_and(|(owner, own)| owner.name() == *name && own == recorded),
                    "{name}: {call} does not route back"
                );
            }
        }
    }
    for successor in ["inspect", "history", "bg", "evidence", "git"] {
        assert!(
            built.contains(&successor),
            "{successor} not built: {built:?}"
        );
    }
}

#[test]
fn mcp_instructions_follow_offered_server_identity_not_tool_name_prefixes() {
    struct ServerTool {
        name: &'static str,
        server: &'static str,
    }
    impl Tool for ServerTool {
        fn name(&self) -> &str {
            self.name
        }
        fn schema(&self) -> Value {
            serde_json::json!({"type":"function","function":{"name":self.name,
                "description":"Test tool","parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            true
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            panic!("selecting server instructions must never execute a tool")
        }
        fn lifecycle_channel(&self) -> Option<&str> {
            Some(self.server)
        }
    }
    let mut registry = Registry::new();
    registry.mcp_instructions = vec![
        ("alpha.test".into(), "first server instructions".into()),
        ("alpha/test".into(), "second server instructions".into()),
        ("unrelated".into(), "unrelated instructions".into()),
    ];
    for (name, server) in [
        ("mcp__alpha_test__run", "alpha.test"),
        ("mcp__alpha_test__run_2", "alpha/test"),
        ("native", "alpha.test"),
    ] {
        registry.register_native(Arc::new(ServerTool { name, server }));
    }
    for (offered, expected) in [
        (vec![], vec![]),
        (vec!["native"], vec![]),
        (vec!["mcp__alpha_test__missing"], vec![]),
        (vec!["mcp__alpha_test__run"], vec!["alpha.test"]),
        (vec!["mcp__alpha_test__run_2"], vec!["alpha/test"]),
        (
            vec!["mcp__alpha_test__run_2", "mcp__alpha_test__run"],
            vec!["alpha.test", "alpha/test"],
        ),
    ] {
        let names: Vec<String> = offered.iter().map(|name| (*name).into()).collect();
        let selected: Vec<&str> = registry
            .mcp_instructions_for(&names)
            .into_iter()
            .map(|(server, _)| server.as_str())
            .collect();
        assert_eq!(selected, expected, "offered {offered:?}");
    }
}

#[test]
fn dap_adapters_share_one_catalog_entry_and_preserve_pinned_owners() {
    let mut registry = Registry::new();
    for tool in crate::tools::code::dap::tools(&Default::default()) {
        registry.register(tool);
    }
    assert_eq!(registry.names().collect::<Vec<_>>(), ["dap"]);
    let schema = registry.get("dap").unwrap().schema();
    crate::tools::surface::catalog::validate_definition(&schema).unwrap();
    let adapters = schema["function"]["parameters"]["properties"]["adapter"]["enum"]
        .as_array()
        .unwrap();
    assert_eq!(adapters.len(), 14);
    assert!(adapters.contains(&serde_json::json!("debugpy")));
    assert_eq!(registry.get("dap_debugpy").unwrap().name(), "dap_debugpy");
    assert!(registry.is_folded("dap_debugpy"));
    let (_, call) = crate::tools::surface::facade::successor_call(
        "dap_debugpy",
        &serde_json::json!({"action":"status"}),
    )
    .unwrap();
    crate::tools::surface::catalog::validate_arguments(&schema, &call).unwrap();
    assert_eq!(
        crate::tools::surface::facade::owner_view("dap", &call),
        ("dap_debugpy".into(), serde_json::json!({"action":"status"}))
    );
    assert_eq!(registry.fork_session().names().collect::<Vec<_>>(), ["dap"]);
}

#[test]
fn running_jobs_and_their_watches_move_to_a_rebuilt_registry() {
    let (_dir, ws) = crate::tools::testkit::workspace();
    let ws = ws.without_sandbox();
    let config = Config::default();
    let allow = |_: &str, _: &[std::path::PathBuf], _: bool| Ok(());
    let ctx = ToolContext {
        workspace: &ws,
        config: &config,
        mutation_authorizer: Some(&allow),
        confirm: None,
    };
    let old = Registry::standard(&config, Vec::new());
    assert!(old.live_processes().is_empty(), "nothing runs yet");
    let bg = Arc::clone(old.owner("bg").unwrap());
    let started = bg.call(
        &ctx,
        &serde_json::json!({"action":"start","name":"server","command":"sleep 30"}),
    );
    assert!(!started.is_error, "{}", started.text);

    let carried = old.live_processes();
    let mut names: Vec<_> = carried.iter().map(|tool| tool.name().to_owned()).collect();
    names.sort();
    assert_eq!(names, ["bg", "monitor"]);
    let mut rebuilt = Registry::standard(&config, Vec::new());
    rebuilt.adopt_live_processes(carried);
    drop(old);
    drop(bg);

    let bg = rebuilt.owner("bg").unwrap();
    assert!(bg.has_live_work(), "the job survived the old registry");
    let listed = bg.call(&ctx, &serde_json::json!({"action":"list"}));
    assert!(listed.text.contains("server"), "{}", listed.text);
    assert!(
        !bg.call(&ctx, &serde_json::json!({"action":"kill","name":"server"}))
            .is_error
    );
}

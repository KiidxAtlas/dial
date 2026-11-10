use super::*;
use std::sync::atomic::AtomicBool;

/// An interactive (non-hermetic) agent over default configuration plus `overlay` and an
/// unreachable endpoint; these tests inspect the offer and prompt only, so no request is made.
pub(crate) fn with_default_agent<T>(overlay: &str, f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
    crate::llm::fake::raise_fd_limit();
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    let mut overlay: toml::Table =
        toml::from_str(&format!("{overlay}\n[improve]\nenabled=false")).unwrap();
    overlay.entry("model").or_insert_with(|| "test".into());
    // Unscripted requests are fixture failures, not transport-retry experiments. Retry-owner
    // tests set their own explicit limits and retain those values.
    overlay
        .entry("retry")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .unwrap()
        .entry("max_attempts")
        .or_insert(toml::Value::Integer(1));
    let config = Config::load(&layout, Some(overlay)).unwrap();
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        std::path::PathBuf::new(),
        5,
    );
    let session = crate::storage::session::Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: config.model.clone(),
        },
    )
    .unwrap();
    let mut agent = Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    f(&mut agent)
}

/// Historical routing fixtures explicitly exercise the retained noncompact configuration.
/// New default-policy regressions use `with_default_agent` instead.
pub(crate) fn with_agent<T>(overlay: &str, f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
    let mut values: toml::Table = toml::from_str(overlay).unwrap();
    values
        .entry("assistance")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .unwrap()
        .entry("compact_tools")
        .or_insert(toml::Value::Boolean(false));
    with_default_agent(&toml::to_string(&values).unwrap(), f)
}

/// Fixed-request reliability fixtures opt out of completion reconciliation explicitly.
/// Default-policy continuation behavior is exercised by the owning completion/advisor tests.
pub(crate) fn with_review_disabled<T>(overlay: &str, f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
    with_agent(
        &format!("{overlay}\n[verification.review]\nenabled=false\n"),
        f,
    )
}

fn schema_bytes(agent: &Agent<'_>, names: &[String]) -> usize {
    serde_json::to_string(&agent.schemas(names).unwrap())
        .unwrap()
        .len()
}

/// System messages with the per-test project directory masked, so prompts built in different
/// temporary projects compare equal.
fn system_text(agent: &Agent<'_>) -> String {
    let root = agent.workspace.root().display().to_string();
    serde_json::to_string(&agent.system_messages())
        .unwrap()
        .replace(&root, "<project>")
}

#[test]
fn normal_offer_includes_supported_eval_without_overriding_operator_choices() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        assert_eq!(
            agent.offered().iter().any(|name| name == "eval"),
            crate::tools::exec::eval::prepare_containment().is_ok(),
        );
    });
    with_agent(
        "[assistance]\nlean_local=false\n[harness]\ntools=['read','bash']",
        |agent| {
            assert!(!agent.offered().iter().any(|name| name == "eval"));
        },
    );
}

#[test]
fn automatic_eval_offer_requires_containment_but_explicit_selections_do_not() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        for ready in [false, true] {
            let offered = agent.offered_with_eval_readiness(false, || ready);
            assert_eq!(offered.iter().any(|name| name == "eval"), ready);
        }
        agent.tool_routing.retained.push("eval".into());
        assert!(
            !agent
                .offered_with_eval_readiness(false, || false)
                .iter()
                .any(|name| name == "eval"),
            "an earlier automatic selection must not outlive failed containment"
        );
        agent.enabled_tools.insert("eval".into());
        assert!(
            agent
                .offered_with_eval_readiness(false, || false)
                .iter()
                .any(|name| name == "eval")
        );
        assert!(
            agent
                .offered_with_eval_readiness(true, || panic!("child selection must not probe"))
                .iter()
                .any(|name| name == "eval")
        );
    });
    with_agent(
        "[assistance]\nlean_local=false\n[harness]\ntools=['read','eval']",
        |agent| {
            assert!(
                agent
                    .offered_with_eval_readiness(false, || panic!(
                        "pinned selection must not probe"
                    ))
                    .iter()
                    .any(|name| name == "eval")
            );
        },
    );
}

#[test]
fn efficient_guidance_shrinks_the_prefix_without_changing_capabilities() {
    let capture = |agent: &mut Agent<'_>| {
        let offered = agent.offered();
        (
            agent.schemas(&offered).unwrap(),
            agent.discoverable_tools(&offered),
            offered,
            system_text(agent),
        )
    };
    let full = with_agent(
        "[assistance]\nlean_local=false\n[efficiency]\nenabled=false",
        capture,
    );
    with_agent("[assistance]\nlean_local=false", |agent| {
        let concise = capture(agent);
        assert_eq!(concise.0, full.0, "callable schemas must not change");
        assert_eq!(concise.1, full.1, "discovery descriptions must not change");
        assert_eq!(concise.2, full.2, "the tool offer must not change");
        eprintln!(
            "built-in system prefix: {} -> {} bytes",
            full.3.len(),
            concise.3.len()
        );
        assert!(concise.3.len() < full.3.len());
        agent.rebuild_system();
        assert_eq!(capture(agent), concise, "prefix must remain cache-stable");
    });
}

#[test]
fn lean_local_profile_shrinks_the_request_and_stays_byte_stable_across_turns() {
    // The bytes a local server prefills: the system messages and the offered tool schemas.
    fn request(agent: &Agent<'_>) -> (String, String, Vec<String>) {
        let offered = agent.offered();
        let tools = serde_json::to_string(&agent.schemas(&offered).unwrap()).unwrap();
        (system_text(agent), tools, offered)
    }
    let default = with_agent("[assistance]\nlean_local=false", |agent| request(agent));
    let lean = with_agent("[assistance]\nlean_local=true", |agent| {
        let first = request(agent);
        // A following turn rebuilds the prefix from the same state and must not change a byte.
        agent.rebuild_system();
        assert_eq!(request(agent), first);
        first
    });
    // Discovery lists the tools it does not offer, so the system text may grow slightly;
    // the prefill as a whole must shrink.
    let (lean_total, default_total) = (
        lean.0.len() + lean.1.len(),
        default.0.len() + default.1.len(),
    );
    assert!(
        lean_total < default_total,
        "prefill {lean_total} vs {default_total}"
    );
    assert!(
        lean.1.len() < default.1.len(),
        "schemas {} vs {}",
        lean.1.len(),
        default.1.len()
    );
    assert!(lean.2.len() < default.2.len(), "offered {:?}", lean.2);
    assert!(!lean.0.contains("# Visual verification"));
    assert!(default.0.contains("# Visual verification"));
}

#[test]
fn lean_local_profile_leaves_hosted_routes_unchanged() {
    let overlay = "[endpoint]\nbase_url='https://api.example.com/v1'\n";
    let default = with_agent(overlay, |agent| (system_text(agent), agent.offered()));
    let lean = with_agent(
        &format!("{overlay}[assistance]\nlean_local=true"),
        |agent| {
            assert!(!agent.lean_local());
            (system_text(agent), agent.offered())
        },
    );
    assert_eq!(lean, default);
}

#[test]
fn window_overflow_uses_the_lean_prompt_even_without_the_local_profile() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        let full = system_text(agent);
        assert!(full.contains("# Visual verification"));
        agent.lean_tools = true;
        agent.rebuild_system();
        let lean = system_text(agent);
        assert!(
            lean.len() < full.len(),
            "Overflow must reduce instruction overhead as well as schemas: {} -> {} bytes",
            full.len(),
            lean.len()
        );
        assert!(!lean.contains("# Visual verification"));
        for tool in ["read", "edit", "finish", "tool_search"] {
            assert!(agent.offered().iter().any(|name| name == tool));
        }
    });
}

#[test]
fn message_is_offered_with_task_whether_or_not_a_worker_is_live() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        // No worker runs here, so the offer must already include `message` with `task`; a live
        // worker cannot change it because the offer no longer reads worker state.
        assert!(!agent.live_task_control().any_running());
        let offered = agent.offered();
        assert!(offered.iter().any(|name| name == "task"), "{offered:?}");
        assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
        let discoverable = agent.discoverable_tools(&offered);
        assert!(!discoverable.iter().any(|(name, _)| name == "message"));
    });
}

#[test]
fn colored_snapcompact_offers_recovery_even_in_a_lean_request() {
    with_agent(
        "[compaction.snapcompact]\noverlay=true\n[assistance]\nlean_local=true",
        |agent| {
            let offered = agent.offered();
            assert!(
                offered.iter().any(|name| name == "session_search"),
                "{offered:?}"
            );
            let schemas = agent.schemas(&offered).unwrap();
            let search = schemas
                .iter()
                .find(|schema| schema["function"]["name"] == "session_search")
                .unwrap();
            assert_eq!(
                search["function"]["parameters"]["properties"]["view"]["enum"],
                json!(["layers"])
            );
            assert_eq!(
                search["function"]["parameters"]["properties"]["format"]["enum"],
                json!(["text", "image"])
            );
        },
    );
}

#[test]
fn default_session_search_schema_does_not_enable_layer_views() {
    with_agent("", |agent| {
        let schema = agent.schemas(&["session_search".into()]).unwrap().remove(0);
        assert!(schema["function"]["parameters"]["properties"]["view"].is_null());
    });
}

#[test]
fn default_offer_defers_overlapping_specialists_but_lists_them_as_loadable() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        let offered = agent.offered();
        let catalog = agent.catalog();
        for core in [
            "read",
            "grep",
            "glob",
            "find",
            "bg",
            "task",
            "memory",
            "tool_search",
        ] {
            assert!(
                offered.iter().any(|name| name == core),
                "{core}: {offered:?}"
            );
        }
        // Saved project understanding and the self-improvement helpers stay loadable only.
        let discoverable_now = agent.discoverable_tools(&offered);
        for loadable in ["codebase_memory", "experience", "edit_preview"] {
            assert!(
                !offered.iter().any(|name| name == loadable),
                "{loadable}: {offered:?}"
            );
            if catalog.iter().any(|name| name == loadable) {
                assert!(
                    discoverable_now.iter().any(|(name, _)| name == loadable),
                    "{loadable}: {discoverable_now:?}"
                );
            }
        }
        let discoverable = agent.discoverable_tools(&offered);
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(prompt.contains("Loadable tools, not in the schema yet"));
        for deferred in tools::DEFERRED_BY_DEFAULT {
            assert!(catalog.iter().any(|name| name == deferred), "{deferred}");
            if *deferred == "git" && offered.iter().any(|name| name == "git") {
                assert!(
                    agent
                        .workspace
                        .root()
                        .ancestors()
                        .any(|dir| dir.join(".git").exists())
                );
                assert!(!discoverable.iter().any(|(name, _)| name == "git"));
                continue;
            }
            assert!(!offered.iter().any(|name| name == deferred), "{offered:?}");
            assert!(
                discoverable.iter().any(|(name, _)| name == deferred),
                "{discoverable:?}"
            );
            let names = prompt
                .split("Names: ")
                .nth(1)
                .and_then(|rest| rest.split("\\n").next())
                .unwrap_or_default();
            assert!(names.split(", ").any(|name| name == *deferred), "{prompt}");
        }
        // Serialized default schema size, before (deferred tools offered) and after.
        let mut before = offered.clone();
        before.extend(
            tools::DEFERRED_BY_DEFAULT
                .iter()
                .map(|name| (*name).to_owned()),
        );
        before.sort();
        before.dedup();
        let (before, after) = (schema_bytes(agent, &before), schema_bytes(agent, &offered));
        eprintln!("default tool schemas: {before} -> {after} bytes");
        assert!(after < before);
    });
}

#[test]
fn unattended_launch_defers_session_helpers_but_lists_them_as_loadable() {
    let helpers = ["task", "message", "todo", "find"];
    with_agent("[assistance]\nlean_local=false", |agent| {
        let attended = agent.offered();
        for helper in helpers {
            assert!(attended.iter().any(|name| name == helper), "{attended:?}");
        }
        agent.modes.launch.unattended = true;
        let offered = agent.offered();
        let discoverable = agent.discoverable_tools(&offered);
        for helper in helpers {
            assert!(!offered.iter().any(|name| name == helper), "{offered:?}");
        }
        for loadable in ["task", "todo", "find", "bg", "finish", "verify"] {
            assert!(
                !offered.iter().any(|name| name == loadable),
                "{loadable}: {offered:?}"
            );
            assert!(
                discoverable.iter().any(|(name, _)| name == loadable),
                "{discoverable:?}"
            );
        }
        for core in [
            "read",
            "edit",
            "write",
            "bash",
            "grep",
            "glob",
            "tool_search",
        ] {
            assert!(
                offered.iter().any(|name| name == core),
                "{core}: {offered:?}"
            );
        }
        // With `bg` deferred, the shell text says it is a Dial tool to load, not a shell builtin.
        assert!(agent.bg_loadable());
        let (before, after) = (
            schema_bytes(agent, &attended),
            schema_bytes(agent, &offered),
        );
        eprintln!("unattended tool schemas: {before} -> {after} bytes");
        assert!(after < before);
    });
    with_agent(
        "[assistance]\nlean_local=false\n[task]\nprefer_subagents=true",
        |agent| {
            agent.modes.launch.unattended = true;
            let offered = agent.offered();
            for kept in ["task", "message"] {
                assert!(offered.iter().any(|name| name == kept), "{offered:?}");
            }
            assert!(!offered.iter().any(|name| name == "working_state"));
        },
    );
}

#[test]
fn unattended_launch_defers_memory_tools_but_keeps_them_loadable() {
    let memory = "[memory]\nbackend='semantic'\n[memory.semantic]\nembedding='off'\n[assistance]\nlean_local=false";
    with_agent(memory, |agent| {
        let attended = agent.offered();
        assert!(attended.iter().any(|name| name == "memory"), "{attended:?}");
        agent.modes.launch.unattended = true;
        let offered = agent.offered();
        let discoverable = agent.discoverable_tools(&offered);
        assert!(!offered.iter().any(|name| name == "memory"), "{offered:?}");
        assert!(
            discoverable.iter().any(|(name, _)| name == "memory"),
            "{discoverable:?}"
        );
        assert!(
            offered.iter().any(|name| name == "tool_search"),
            "{offered:?}"
        );
    });
}

pub(in crate::agent) fn load_tools(agent: &mut Agent<'_>, arguments: Value) -> ToolOutput {
    struct Quiet;
    impl Ui for Quiet {}
    let mut backend = agent.backend.take().unwrap();
    let output = agent.dispatch(
        &mut backend,
        &ToolCall {
            id: "call-load".into(),
            name: "tool_search".into(),
            arguments,
            malformed_arguments: false,
        },
        &mut Quiet,
    );
    agent.backend = Some(backend);
    output
}

#[test]
fn tool_load_offers_a_deferred_tool_next_round_and_unload_defers_it_again() {
    with_agent("", |agent| {
        let names: Vec<&str> = tools::DEFERRED_BY_DEFAULT.to_vec();
        let out = load_tools(agent, json!({"names": names}));
        assert!(!out.is_error, "{}", out.text);
        let receipt = out.data.as_ref().unwrap();
        assert_eq!(receipt["selection_persisted"], true);
        assert_eq!(receipt["tools"].as_array().unwrap().len(), names.len());
        for entry in receipt["tools"].as_array().unwrap() {
            assert_eq!(entry["action"], "load");
            assert_eq!(entry["status"], "loaded_next_request");
            assert_eq!(entry["enabled_next_round"], true);
            assert!(entry["readiness"]["state"].is_string(), "{entry:?}");
        }
        let offered = agent.offered();
        for deferred in tools::DEFERRED_BY_DEFAULT {
            assert!(offered.iter().any(|name| name == deferred), "{offered:?}");
        }
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(!prompt.contains("  - lsp: "), "{prompt}");
        let out = load_tools(agent, json!({"unload": ["lsp"]}));
        assert!(!out.is_error, "{}", out.text);
        let entry = &out.data.as_ref().unwrap()["tools"][0];
        assert_eq!(entry["name"], "lsp");
        assert_eq!(entry["action"], "unload");
        assert_eq!(entry["enabled_next_round"], false);
        assert_eq!(entry["status"], "not_loaded");
        assert_eq!(entry["retained_by_policy"], false);
        assert!(entry["next"].as_str().unwrap().contains("tool_search"));
        assert!(out.text.contains("not offered [lsp]"), "{}", out.text);
        let offered = agent.offered();
        assert!(!offered.iter().any(|name| name == "lsp"));
        assert!(offered.iter().any(|name| name == "grep"));
    });
}

#[test]
fn tool_load_reports_core_retention_and_deduplicates_selection_receipts() {
    for overlay in ["", "[assistance]\ncompact_tools=true"] {
        with_agent(overlay, |agent| {
            let output = load_tools(agent, json!({"names":["read", "read", "lsp"]}));
            assert!(!output.is_error, "{}", output.text);
            assert_eq!(
                output.data.as_ref().unwrap()["tools"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            let output = load_tools(agent, json!({"unload":["read", "read", "lsp"]}));
            assert!(!output.is_error, "{}", output.text);
            let entries = output.data.as_ref().unwrap()["tools"].as_array().unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0]["name"], "lsp");
            assert_eq!(entries[0]["enabled_next_round"], false);
            assert_eq!(entries[1]["name"], "read");
            assert_eq!(entries[1]["status"], "already_offered");
            assert_eq!(entries[1]["enabled_next_round"], true);
            assert_eq!(entries[1]["retained_by_policy"], true);
            assert!(
                output.text.contains("offered [read]; not offered [lsp]"),
                "{}",
                output.text
            );
            assert!(agent.offered().contains(&"read".to_owned()));
            assert!(!agent.offered().contains(&"lsp".to_owned()));
        });
    }
}

#[test]
fn tool_load_refuses_overlap_before_changing_pins_discovery_or_routing() {
    with_agent("[assistance]\ncompact_tools=true", |agent| {
        assert!(!load_tools(agent, json!({"names":["lsp"]})).is_error);
        let before = agent.session.control_state();
        let offered = agent.offered();
        for arguments in [
            json!({"names":["lsp"], "unload":["lsp"]}),
            json!({"names":["read", "lsp", "read"], "unload":["read", "lsp"]}),
        ] {
            let output = load_tools(agent, arguments);
            assert!(output.is_error, "{}", output.text);
            assert!(
                output.text.contains("cannot load and unload lsp"),
                "{}",
                output.text
            );
            assert!(output.data.is_none());
            assert_eq!(agent.session.control_state(), before);
            assert_eq!(agent.offered(), offered);
            assert!(!agent.tool_routing.is_suppressed("lsp"));
            assert!(!agent.enabled_tools.contains("read"));
        }
    });
}

#[test]
fn tool_load_persistence_failure_cannot_report_success_or_change_effective_offer() {
    with_agent("[assistance]\ncompact_tools=true", |agent| {
        assert!(!load_tools(agent, json!({"names":["lsp"]})).is_error);
        let path = agent.session.path().unwrap().to_owned();
        let snapshot = crate::storage::session::Session::read(&path).unwrap();
        agent.session = snapshot;
        let before = agent.session.control_state();
        let offered = agent.offered();
        let output = load_tools(agent, json!({"names":["git"], "unload":["lsp"]}));
        assert!(output.is_error, "{}", output.text);
        assert!(
            output.text.contains("persisting enabled tools"),
            "{}",
            output.text
        );
        assert!(output.data.is_none());
        assert_eq!(agent.session.control_state(), before);
        assert_eq!(agent.offered(), offered);
        assert!(!agent.enabled_tools.contains("git"));
        assert!(!agent.tool_routing.is_suppressed("lsp"));
    });
}

#[test]
fn tool_load_can_unload_ineligible_activation_without_promising_a_reload() {
    with_agent("[assistance]\ncompact_tools=true", |agent| {
        let output = load_tools(agent, json!({"names":["lsp"]}));
        assert!(!output.is_error, "{}", output.text);
        agent.modes.launch.tools = Some(["tool_search".to_owned()].into());
        let output = load_tools(agent, json!({"unload":["lsp"]}));
        assert!(!output.is_error, "{}", output.text);
        let entry = &output.data.as_ref().unwrap()["tools"][0];
        assert_eq!(entry["eligible"], false);
        assert_eq!(entry["enabled_next_round"], false);
        assert_eq!(entry["status"], "not_loaded");
        assert!(
            entry["next"]
                .as_str()
                .unwrap()
                .contains("loading cannot bypass this boundary")
        );
        assert!(!agent.enabled_tools.contains("lsp"));
        assert!(!agent.offered().contains(&"lsp".to_owned()));
    });
}

#[test]
fn pinned_harness_selection_keeps_deferred_tools_offered() {
    with_agent(
        "[harness]\ntools=['read','edit','bash','memory','lsp']\n[assistance]\nlean_local=false",
        |agent| {
            let offered = agent.offered();
            assert!(offered.iter().any(|name| name == "lsp"), "{offered:?}");
        },
    );
}

#[test]
fn selected_memory_tools_stay_in_compact_lean_and_fallback_offers() {
    let memory = "[memory]\nbackend='semantic'\n[memory.semantic]\nembedding='off'\n";
    for profile in [
        "[assistance]\ncompact_tools=true",
        "[assistance]\nlean_local=true",
    ] {
        with_agent(&format!("{memory}{profile}"), |agent| {
            assert!(agent.tool_discovery(), "{profile}");
            if agent.stable_tool_offer() {
                assert!(!agent.offered().contains(&"memory".to_owned()));
                assert!(!load_tools(agent, json!({"names":["memory"]})).is_error);
            }
            let offered = agent.offered();
            assert!(
                offered.iter().any(|name| name == "memory"),
                "{profile}: {offered:?}"
            );
            // The fallback used when the full set does not fit the window keeps it too.
            agent.lean_tools = true;
            let offered = agent.offered();
            assert!(
                offered.iter().any(|name| name == "memory"),
                "{profile}: {offered:?}"
            );
            // The obsolete split tool names are gone from the catalog.
            for obsolete in ["recall", "retain", "memory_update", "forget"] {
                assert!(!agent.catalog().iter().any(|name| name == obsolete));
            }
            agent.schemas(&offered).unwrap();
        });
    }
    // Provenance's own memory tool is protected the same way.
    with_agent("[assistance]\ncompact_tools=true", |agent| {
        assert!(!load_tools(agent, json!({"names":["memory"]})).is_error);
        agent.lean_tools = true;
        let offered = agent.offered();
        assert!(offered.iter().any(|name| name == "memory"), "{offered:?}");
    });
}

#[test]
fn autolearn_offers_learn_up_front_and_guides_the_root_agent() {
    with_agent(
        "[memory]\nbackend='local'\nautolearn=true\n[assistance]\nlean_local=false",
        |agent| {
            let offered = agent.offered();
            assert!(offered.iter().any(|name| name == "memory"), "{offered:?}");
            assert!(system_text(agent).contains("Auto-learn is enabled"));
        },
    );
    with_agent("[assistance]\nlean_local=false", |agent| {
        assert!(!system_text(agent).contains("Auto-learn is enabled"));
    });
}

#[test]
fn memory_guidance_follows_the_tools_the_backend_offers() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        assert!(system_text(agent).contains("action recall searches facts"));
    });
    with_agent("[memory]\nbackend='semantic'", |agent| {
        assert!(system_text(agent).contains("Action retain saves a durable fact"));
    });
    with_agent("[memory]\nbackend='sharpshooter'", |agent| {
        let text = system_text(agent);
        assert!(text.contains("saved project decisions"), "{text}");
        assert!(!text.contains("Action retain saves"));
    });
    for off in ["[memory]\nbackend='off'", "[memory]\nbackend='local'"] {
        with_agent(off, |agent| {
            assert!(!system_text(agent).contains("Memory: `memory`"), "{off}");
        });
    }
}

#[test]
fn discovered_working_set_is_bounded_persisted_and_separate_from_pins() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            let base = agent.offered();
            let limit = Agent::DISCOVERED_TOOL_LIMIT;
            let candidates: Vec<String> = agent
                .catalog()
                .into_iter()
                .filter(|name| {
                    !base.contains(name) && !matches!(name.as_str(), "tool_search" | "tool_load")
                })
                .collect();
            assert!(candidates.len() > limit, "{candidates:?}");
            for name in &candidates {
                agent.activate_tools(std::slice::from_ref(name)).unwrap();
            }
            let working = agent.discovered_tools();
            assert_eq!(working.len(), limit);
            assert_eq!(working, candidates[candidates.len() - limit..].to_vec());
            let offered = agent.offered();
            assert!(working.iter().all(|name| offered.contains(name)));
            assert!(
                !offered.contains(&candidates[0]),
                "evicted tools stop being offered"
            );
            // Persisted in the session control state, which a resumed session restores from.
            assert_eq!(
                agent.session.control_state()["discovered_tools"],
                json!(working)
            );
            // A pinned tool is never recorded as discovered and survives discovery eviction.
            let pin = candidates[0].clone();
            agent.enabled_tools.insert(pin.clone());
            agent.activate_tools(std::slice::from_ref(&pin)).unwrap();
            assert_eq!(agent.discovered_tools(), working);
            assert!(agent.offered().contains(&pin));
        },
    );
}

#[test]
fn compact_discovery_pins_and_unloads_do_not_leave_hidden_activation_state() {
    with_agent(
        "[assistance]\ncompact_tools=false\nlean_local=true",
        |agent| {
            struct Quiet;
            impl Ui for Quiet {}
            let name = "lsp".to_owned();
            agent.activate_tools(std::slice::from_ref(&name)).unwrap();
            let mut backend = agent.backend.take().unwrap();
            for arguments in [json!({"names":[name]}), json!({"unload":[name]})] {
                let call = ToolCall {
                    id: "pin-discovered".into(),
                    name: "tool_search".into(),
                    arguments,
                    malformed_arguments: false,
                };
                let output = agent.dispatch(&mut backend, &call, &mut Quiet);
                assert!(!output.is_error, "{}", output.text);
                assert!(!agent.discovered_tools().contains(&name));
            }
            assert!(!agent.offered().contains(&name));
            assert!(!agent.enabled_tools.contains(&name));
            assert_eq!(agent.session.control_state()["discovered_tools"], json!([]));
        },
    );
}

#[test]
fn default_stale_reduction_precedes_automatic_summary_pressure() {
    with_agent("context_window=128000", |agent| {
        let selection = agent.auth_config.selection();
        let threshold = agent
            .config
            .compaction
            .effective_threshold(
                crate::config::compaction::CompactionIdentity {
                    provider: &selection.provider,
                    model: &selection.model,
                    agent: None,
                },
                agent.budget_window(),
            )
            .unwrap();
        // Cross the current default trigger without filling the physical context window.
        // The default is reserve-relative, not the historical 75%-of-window trigger.
        let old_bytes = usize::try_from((threshold + 1024) * 4).unwrap() - 160_000;
        let mut messages = vec![
            json!({"role":"user","content":"task"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"old","content":"x".repeat(old_bytes),"dial_evidence":{"artifact":"artifact://old","observation":"same-range"}}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"new","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"new","content":"y".repeat(160_000),"dial_evidence":{"artifact":"artifact://new","observation":"same-range"}}),
        ];
        let raw = crate::llm::wire::estimate_parts(&messages, &[]);
        assert!(raw > threshold);
        assert!(raw < u64::from(agent.budget_window()));
        let mut packet = crate::agent::workers::task_packet::PacketCache::default();
        let mut memo = crate::agent::workflow::evidence::Supersession::default();
        agent
            .project_history(&mut messages, &[], 0, &mut packet, &mut memo, None)
            .unwrap();
        let old = messages
            .iter()
            .find(|m| m["tool_call_id"] == "old")
            .unwrap();
        assert!(
            old["content"]
                .as_str()
                .unwrap()
                .starts_with("[Older read result omitted")
        );
        assert!(crate::llm::wire::estimate_parts(&messages, &[]) < threshold);
    });
}

#[test]
fn pruning_protects_the_capped_budget_tail_not_the_raw_model_window() {
    with_agent(
        "context_window=1000000\n[efficiency]\nhistory_token_cap=16000",
        |agent| {
            assert_eq!(agent.budget_window(), 16_000);
            let mut messages = vec![
                json!({"role":"user","content":"task"}),
                json!({"role":"assistant","content":"","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
                json!({"role":"tool","tool_call_id":"old","content":"x".repeat(80_000),"dial_evidence":{"artifact":"artifact://old","observation":"k"}}),
                json!({"role":"assistant","content":"","tool_calls":[{"id":"new","function":{"name":"read","arguments":"{}"}}]}),
                json!({"role":"tool","tool_call_id":"new","content":"y".repeat(20_000),"dial_evidence":{"artifact":"artifact://new","observation":"k"}}),
            ];
            let mut packet = crate::agent::workers::task_packet::PacketCache::default();
            let mut superseded = crate::agent::workflow::evidence::Supersession::default();
            agent
                .project_history(&mut messages, &[], 0, &mut packet, &mut superseded, None)
                .unwrap();
            let old = messages
                .iter()
                .find(|m| m["tool_call_id"] == "old")
                .and_then(|m| m["content"].as_str())
                .unwrap();
            assert!(
                old.starts_with("[Older read result omitted"),
                "a 1M-token model under a 16k history cap must not protect a 200k-token tail: {old}"
            );
        },
    );
}

/// Local routes lean by default at any window; `auto` follows the planned window; an explicit value
/// overrides the window, and hosted routes never lean.
#[test]
fn lean_local_auto_follows_the_planned_window_and_explicit_values_override_it() {
    let lean = |overlay: &str| with_agent(overlay, |agent| agent.lean_local());
    // On is the default: a local route (no endpoint, or loopback) leans whatever its window.
    assert!(lean("context_window=32768"));
    assert!(lean("context_window=262144"));
    assert!(lean(
        "context_window=8192\n[endpoint]\nbase_url='http://127.0.0.1:9/v1'"
    ));
    assert!(lean(
        "context_window=32768\n[assistance]\nlean_local='auto'"
    ));
    assert!(!lean(
        "context_window=65536\n[assistance]\nlean_local='auto'"
    ));
    // Explicit false opts out even on a small local window; explicit true forces a large one.
    assert!(!lean("context_window=8192\n[assistance]\nlean_local=false"));
    assert!(lean("context_window=65536\n[assistance]\nlean_local=true"));
    // Hosted routes never lean, in any mode.
    for mode in [
        "",
        "[assistance]\nlean_local=true",
        "[assistance]\nlean_local='auto'",
    ] {
        let overlay = format!(
            "context_window=8192\n[endpoint]\nbase_url='https://api.example.com/v1'\n{mode}"
        );
        assert!(!lean(&overlay), "{overlay}");
    }
}

/// On a single-request local server the lean profile keeps `task` and `message` out of the first
/// request and the delegation section with them; loading `task` brings all three back.
#[test]
fn lean_single_slot_server_defers_delegation_until_loaded() {
    let overlay = "[endpoint]\nbase_url='http://127.0.0.1:9/v1'\n[assistance]\nlean_local=true";
    with_agent(overlay, |agent| {
        assert!(agent.lean_local());
        let offered = agent.offered();
        assert!(!offered.iter().any(|name| name == "task"), "{offered:?}");
        assert!(!offered.iter().any(|name| name == "message"), "{offered:?}");
        assert!(
            agent
                .discoverable_tools(&offered)
                .iter()
                .any(|(name, _)| name == "task")
        );
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(!prompt.contains("# Delegation"), "{prompt}");
        agent.activate_tools(&["task".to_owned()]).unwrap();
        let offered = agent.offered();
        assert!(offered.iter().any(|name| name == "task"), "{offered:?}");
        assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(prompt.contains("# Delegation"));
    });
}

/// A server with a concurrency limit above one can run fan-out, so the lean profile offers it up front.
#[test]
fn lean_multi_slot_server_keeps_delegation_offered() {
    let overlay = "[endpoint]\nbase_url='http://127.0.0.1:9/v1'\n[assistance]\nlean_local=true\n[task.provider_concurrency]\nendpoint=2";
    with_agent(overlay, |agent| {
        assert!(agent.lean_local());
        let offered = agent.offered();
        assert!(offered.iter().any(|name| name == "task"), "{offered:?}");
        assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(prompt.contains("# Delegation"));
    });
}

/// The small tier offers the minimal set up front on local routes at or under `small_window`;
/// larger windows, `small_window = 0`, a disabled local profile, `lean_local = false` and hosted
/// routes keep their offer.
#[test]
fn local_small_tier_offers_the_minimal_set_only_where_it_applies() {
    use crate::agent::model::local_profile::Tier;
    let minimal = |offered: &[String]| {
        offered
            .iter()
            .all(|name| Agent::LEAN_TOOLS.contains(&name.as_str()) || name == "memory")
    };
    with_agent("context_window=16384", |agent| {
        assert_eq!(agent.local_tier(), Some(Tier::Small));
        let offered = agent.offered();
        assert!(minimal(&offered), "{offered:?}");
        assert!(offered.iter().any(|name| name == "edit"), "{offered:?}");
        // Every other tool stays reachable through the loaders.
        assert!(
            offered.iter().any(|name| name == "tool_search"),
            "{offered:?}"
        );
    });
    for (overlay, tier) in [
        ("context_window=16385", Some(Tier::Lean)),
        ("context_window=65536", Some(Tier::Lean)),
        (
            "context_window=65536\n[assistance]\nlean_local='auto'",
            Some(Tier::Full),
        ),
        (
            "context_window=16384\n[assistance.local]\nsmall_window=0",
            Some(Tier::Lean),
        ),
        (
            "context_window=16384\n[assistance.local]\nenabled=false",
            Some(Tier::Lean),
        ),
        (
            "context_window=16384\n[assistance]\nlean_local=false",
            Some(Tier::Full),
        ),
        (
            "context_window=16384\n[endpoint]\nbase_url='https://api.example.com/v1'",
            None,
        ),
    ] {
        with_agent(overlay, |agent| {
            assert_eq!(agent.local_tier(), tier, "{overlay}");
            let offered = agent.offered();
            assert!(!minimal(&offered), "{overlay}: {offered:?}");
        });
    }
}

/// Local routes default to the replace grammar; explicit pins, `inherit`, hosted routes and a
/// disabled profile keep `[edit] mode`.
#[test]
fn local_routes_default_to_replace_edits_unless_pinned() {
    use crate::config::types::EditMode;
    let mode = |overlay: &str| with_agent(overlay, |agent| agent.edit_mode());
    assert_eq!(mode(""), EditMode::Replace);
    assert_eq!(
        mode("[endpoint]\nbase_url='http://127.0.0.1:9/v1'"),
        EditMode::Replace
    );
    assert_eq!(
        mode("[assistance.local]\nedit_mode='apply_patch'"),
        EditMode::ApplyPatch
    );
    assert_eq!(
        mode("[assistance.local]\nedit_mode='inherit'"),
        EditMode::Hashline
    );
    assert_eq!(
        mode("[assistance.local]\nenabled=false"),
        EditMode::Hashline
    );
    assert_eq!(mode("[harness]\nedit_mode='patch'"), EditMode::Patch);
    assert_eq!(
        mode("[edit.model_variants]\ntest='sloppy'"),
        EditMode::Sloppy
    );
    assert_eq!(
        mode("[endpoint]\nbase_url='https://api.example.com/v1'"),
        EditMode::Hashline
    );
}

/// Failing rounds escalate one step at a time: the minimal set and the short prompt, then the
/// replace grammar. A success resets the streak. A reached step takes effect at a request boundary
/// or at a later turn start, and is remembered for the model so a new session starts from it.
#[test]
fn local_escalation_steps_up_on_failing_rounds_and_is_remembered() {
    use crate::config::types::EditMode;
    let overlay = "context_window=65536\n[assistance.local]\nedit_mode='inherit'";
    with_agent(overlay, |agent| {
        let ui = &mut crate::agent::NullUi;
        agent.seed_local_escalation();
        let full = agent.offered();
        let prompt = system_text(agent);
        assert!(!agent.minimal_tools());
        assert_eq!(agent.edit_mode(), EditMode::Hashline);
        assert!(!agent.observe_local_round(true, ui));
        assert!(!agent.observe_local_round(false, ui), "a success resets");
        assert!(!agent.observe_local_round(true, ui));
        assert!(agent.observe_local_round(true, ui));
        // The reached step waits for a request boundary or the next turn start.
        assert!(!agent.minimal_tools());
        assert_eq!(agent.offered(), full);
        assert_eq!(system_text(agent), prompt);
        agent.seed_local_escalation();
        assert!(agent.minimal_tools());
        assert!(agent.offered().len() < full.len());
        assert!(!system_text(agent).contains("# Visual verification"));
        assert_eq!(agent.edit_mode(), EditMode::Hashline);
        assert!(!agent.observe_local_round(true, ui));
        assert!(agent.observe_local_round(true, ui));
        assert_eq!(
            agent.edit_mode(),
            EditMode::Hashline,
            "not before a boundary"
        );
        agent.seed_local_escalation();
        assert_eq!(agent.edit_mode(), EditMode::Replace);
        assert_eq!(agent.workspace.edit_mode(), EditMode::Replace);
        // Nothing is stricter than the last step.
        assert!(!agent.observe_local_round(true, ui));
        assert!(!agent.observe_local_round(true, ui));
        // A later session of the same model starts from the remembered step.
        agent.local_escalation = Default::default();
        agent.workspace = agent.workspace.clone().with_edit_mode(EditMode::Hashline);
        agent.rebuild_system();
        assert!(!agent.minimal_tools());
        agent.seed_local_escalation();
        assert!(agent.minimal_tools());
        assert_eq!(agent.workspace.edit_mode(), EditMode::Replace);
    });
    // Both steps reached before a boundary also apply together at a later turn start.
    with_agent(overlay, |agent| {
        let ui = &mut crate::agent::NullUi;
        for _ in 0..4 {
            agent.observe_local_round(true, ui);
        }
        assert!(!agent.minimal_tools());
        assert_eq!(agent.edit_mode(), EditMode::Hashline);
        agent.seed_local_escalation();
        assert!(agent.minimal_tools());
        assert_eq!(agent.edit_mode(), EditMode::Replace);
    });
    // A pinned grammar stops at the first step, and escalation is off on hosted routes.
    with_agent(
        "context_window=65536\n[harness]\nedit_mode='hashline'",
        |agent| {
            let ui = &mut crate::agent::NullUi;
            assert!(!agent.observe_local_round(true, ui));
            assert!(agent.observe_local_round(true, ui));
            assert!(!agent.observe_local_round(true, ui));
            assert!(!agent.observe_local_round(true, ui));
            assert_eq!(agent.edit_mode(), EditMode::Hashline);
        },
    );
    with_agent(
        "[endpoint]\nbase_url='https://api.example.com/v1'",
        |agent| {
            let ui = &mut crate::agent::NullUi;
            for _ in 0..4 {
                assert!(!agent.observe_local_round(true, ui));
            }
        },
    );
}

/// `sampling = "model"` leaves a local server's own defaults in charge (no temperature is sent);
/// a pinned temperature, `sampling = "harness"` and hosted routes keep Dial's temperature.
#[test]
fn local_sampling_defers_to_the_server_unless_pinned() {
    let local = "[endpoint]\nbase_url='http://127.0.0.1:9/v1'";
    let apply = |overlay: &str, temperature: f64| {
        with_agent(overlay, |agent| {
            let mut profile = None;
            let source = agent.apply_local_sampling(&mut profile, temperature);
            (source, profile.map(|p| p.capabilities.temperature))
        })
    };
    assert_eq!(
        apply(local, 0.2),
        (Some("local server defaults"), Some(false))
    );
    assert_eq!(
        apply(&format!("{local}\n[harness]\ntemperature=0.7"), 0.7),
        (None, None)
    );
    assert_eq!(
        apply(
            &format!("{local}\n[assistance.local]\nsampling='harness'"),
            0.2
        ),
        (None, None)
    );
    assert_eq!(
        apply("[endpoint]\nbase_url='https://api.example.com/v1'", 0.2),
        (None, None)
    );
}

#[test]
fn local_sampling_sends_greedy_for_do_sample_false_unless_pinned() {
    // Use a real model directory so metadata follows the same path as a local request.
    let model = tempfile::tempdir().unwrap();
    std::fs::write(
        model.path().join("generation_config.json"),
        r#"{"do_sample":false,"temperature":0.7,"top_k":40}"#,
    )
    .unwrap();
    let overlay = format!(
        "model={}",
        toml::Value::String(model.path().to_str().unwrap().into())
    );
    with_agent(&overlay, |agent| {
        let mut profile = None;
        assert_eq!(
            agent.apply_local_sampling(&mut profile, 0.2),
            Some("generation_config.json")
        );
        let mut body = serde_json::json!({"temperature":0.2});
        profile.unwrap().apply_openai(&mut body);
        assert_eq!(body["temperature"], 0.0);

        let mut profile = None;
        assert_eq!(agent.apply_local_sampling(&mut profile, 0.7), None);
        assert!(
            profile.is_none(),
            "An explicit harness temperature must win"
        );
    });
}

/// The reasoning cap applies on lean local tiers only and scales with the planned window.
#[test]
fn local_reasoning_cap_follows_the_lean_tiers() {
    let cap = |overlay: &str| with_agent(overlay, |agent| agent.local_reasoning_cap());
    for (window, bytes) in [(16384, 6553), (32768, 13107), (65536, 26214)] {
        assert_eq!(cap(&format!("context_window={window}")), Some(bytes));
    }
    assert_eq!(
        cap("context_window=16384\n[assistance.local]\nreasoning_share=50"),
        Some(32768)
    );
    assert_eq!(
        cap("context_window=65536\n[assistance]\nlean_local='auto'"),
        None
    );
    assert_eq!(
        cap("context_window=16384\n[assistance.local]\nreasoning_share=0"),
        None
    );
    assert_eq!(
        cap("context_window=16384\n[endpoint]\nbase_url='https://api.example.com/v1'"),
        None
    );
}

#[test]
fn restrictions_apply_to_discovery_loading_and_late_offers() {
    for mode in ["plan", "vibe", "allowlist"] {
        with_agent("", |agent| {
            match mode {
                "plan" => agent.plan_mode = true,
                "vibe" => agent.modes.vibe = true,
                _ => {
                    agent.modes.launch.tools =
                        Some(["read".to_owned(), "tool_search".to_owned()].into())
                }
            }
            let offered = agent.offered();
            if mode == "plan" {
                assert!(
                    offered.iter().any(|name| name == "exit_plan"),
                    "{offered:?}"
                );
                if offered.iter().any(|name| name == "task") {
                    assert!(offered.iter().any(|name| name == "message"), "{offered:?}");
                }
            }
            assert!(
                !offered.iter().any(|name| name == "bash"),
                "{mode}: {offered:?}"
            );
            assert!(
                !agent
                    .discoverable_tools(&offered)
                    .iter()
                    .any(|(name, _)| name == "bash")
            );
            if mode != "plan" {
                assert!(
                    !offered.iter().any(|name| name == "bg"),
                    "{mode}: {offered:?}"
                );
            }
            assert!(agent.activate_tools(&["bash".into()]).is_err());
            struct Quiet;
            impl Ui for Quiet {}
            let mut backend = agent.backend.take().unwrap();
            let out = agent.dispatch(
                &mut backend,
                &ToolCall {
                    id: "restricted-load".into(),
                    name: "tool_search".into(),
                    arguments: json!({"names":["bash"]}),
                    malformed_arguments: false,
                },
                &mut Quiet,
            );
            assert!(out.is_error, "{mode}: {}", out.text);
            assert!(!agent.enabled_tools.contains("bash"));
        });
    }
}

#[test]
fn pending_background_work_returns_to_the_model_without_a_hidden_wait() {
    struct Job(std::sync::atomic::AtomicUsize);
    impl crate::tools::integrations::ext::Tool for Job {
        fn name(&self) -> &str {
            "pending_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"pending_probe","description":"pending job fixture","parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            ToolOutput::ok("unused".into())
        }
        fn awaited_work(&self) -> Vec<String> {
            // A legacy spin/wait would consume the second observation and incorrectly finish.
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
                vec!["pending-probe still running".into()]
            } else {
                vec![]
            }
        }
    }
    with_agent("", |agent| {
        struct Quiet;
        impl Ui for Quiet {}
        agent
            .ext
            .try_register(Arc::new(Job(std::sync::atomic::AtomicUsize::new(0))))
            .unwrap();
        assert!(agent.await_background_jobs(&mut Quiet));
        assert!(
            agent
                .background_notices
                .iter()
                .any(|notice| notice.contains("pending-probe"))
        );
    });
}

#[test]
fn unattended_run_is_told_once_that_session_services_end_with_it() {
    struct Service;
    impl crate::tools::integrations::ext::Tool for Service {
        fn name(&self) -> &str {
            "service_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"service_probe","description":"service fixture","parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            ToolOutput::ok("unused".into())
        }
        fn session_work(&self) -> Vec<String> {
            vec!["api (python server.py)".into()]
        }
    }
    with_agent("", |agent| {
        struct Quiet;
        impl Ui for Quiet {}
        agent.ext.try_register(Arc::new(Service)).unwrap();
        // An attended session keeps its services for the next turn: nothing to say.
        assert!(!agent.await_background_jobs(&mut Quiet));
        assert!(agent.background_notices.is_empty());
        agent.modes.launch.unattended = true;
        assert!(agent.await_background_jobs(&mut Quiet));
        let notice = agent.background_notices.join("\n");
        assert!(notice.contains("api (python server.py)"), "{notice}");
        assert!(notice.contains("persist=true"), "{notice}");
        // Reported once: the next turn end, or `finish`, is not held again by the same job.
        agent.background_notices.clear();
        assert!(!agent.await_background_jobs(&mut Quiet));
        assert!(agent.exit_bound_jobs_notice().is_none());
    });
}

#[test]
fn live_job_output_reaches_the_ui_without_a_matching_foreground_tool() {
    struct Job;
    impl crate::tools::integrations::ext::Tool for Job {
        fn name(&self) -> &str {
            "output_probe"
        }
        fn schema(&self) -> Value {
            json!({"type":"function","function":{"name":"output_probe","description":"job output fixture","parameters":{"type":"object","properties":{}}}})
        }
        fn needs_approval(&self, _: &Value) -> bool {
            false
        }
        fn call(&self, _: &ToolContext, _: &Value) -> ToolOutput {
            ToolOutput::ok("done".into())
        }
        fn drain_notifications(&self) -> Vec<crate::tools::integrations::ext::ToolEvent> {
            vec![
                crate::tools::integrations::ext::ToolEvent {
                    kind: "background_output".into(),
                    name: "job-identity".into(),
                    text: "pipe delta".into(),
                    artifact: None,
                },
                crate::tools::integrations::ext::ToolEvent {
                    kind: "background_progress".into(),
                    name: "pty-identity".into(),
                    text: "screen".into(),
                    artifact: None,
                },
            ]
        }
        fn call_streaming(
            &self,
            ctx: &ToolContext,
            args: &Value,
            on_event: &(dyn Fn(crate::tools::integrations::ext::ToolEvent) + Sync),
            _: &AtomicBool,
        ) -> ToolOutput {
            for event in self.drain_notifications() {
                on_event(event);
            }
            self.call(ctx, args)
        }
    }
    #[derive(Default)]
    struct Recording(Vec<(String, String, bool)>);
    impl Ui for Recording {
        fn background_output(&mut self, name: &str, text: &str, replace: bool) {
            self.0.push((name.into(), text.into(), replace));
        }
        fn tool_output(&mut self, _: &str, _: &str) {
            panic!("job output must not route to a foreground tool");
        }
    }
    with_agent("", |agent| {
        agent.ext.try_register(Arc::new(Job)).unwrap();
        let mut ui = Recording::default();
        agent.pump_subagents(&mut ui);
        assert!(ui.0.contains(&("job-identity".into(), "pipe delta".into(), false)));
        assert!(ui.0.contains(&("pty-identity".into(), "screen".into(), true)));
        ui.0.clear();
        let out = agent
            .run_background_tool("output_probe", &json!({}), &mut ui, None)
            .unwrap();
        assert!(!out.is_error, "{}", out.text);
        assert!(ui.0.contains(&("job-identity".into(), "pipe delta".into(), false)));
        assert!(ui.0.contains(&("pty-identity".into(), "screen".into(), true)));
    });
}

#[test]
fn a_worker_message_to_an_idle_parent_is_surfaced_and_queued_for_its_next_request() {
    #[derive(Default)]
    struct Notices(Vec<String>);
    impl Ui for Notices {
        fn notice(&mut self, message: &str) {
            self.0.push(message.to_owned());
        }
    }
    with_agent("", |agent| {
        agent
            .live_tasks
            .team()
            .enlist("task-1", "scout", Box::new(|| {}))
            .unwrap();
        let receipt = agent
            .live_tasks
            .team()
            .send("task-1", "parent", "found the bug\nsecond line")
            .unwrap();
        assert_eq!(receipt.queued, ["parent"]);
        // No turn runs: the idle controller's pump is what surfaces the message.
        assert!(agent.background_notices.is_empty());
        let mut ui = Notices::default();
        agent.pump_subagents(&mut ui);
        assert_eq!(
            ui.0,
            ["Message from task-1 (scout) to parent: found the bug"]
        );
        assert_eq!(agent.background_notices.len(), 1);
        assert!(
            agent.background_notices[0].contains("not instructions")
                && agent.background_notices[0].contains("second line")
        );
        // No request was started and the letter is delivered once.
        agent.pump_subagents(&mut ui);
        assert_eq!(ui.0.len(), 1);
        assert_eq!(agent.background_notices.len(), 1);
    });
}

#[test]
fn the_parents_message_tool_reports_receipts_and_marks_finished_workers() {
    with_agent("", |agent| {
        let team = agent.live_tasks.team().clone();
        team.enlist("task-1", "scout", Box::new(|| {})).unwrap();
        let reply = agent
            .live_tasks
            .message_call(&json!({"to":"task-1","text":"hello"}))
            .unwrap();
        assert_eq!(
            reply,
            json!({"receipts":[{"to":"task-1","status":"queued"}]})
        );
        team.retire("task-1", "completed");
        let finished = agent
            .live_tasks
            .message_call(&json!({"to":"task-1","text":"late"}))
            .unwrap();
        assert_eq!(finished["receipts"][0]["status"], "finished", "{finished}");
        let unknown = agent
            .live_tasks
            .message_call(&json!({"to":"task-7","text":"late"}))
            .unwrap_err();
        assert!(
            unknown.contains("no live recipient named task-7"),
            "{unknown}"
        );
    });
}

#[test]
fn unrelated_coding_turn_does_not_pay_for_web_schemas() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        agent.ext.register(Arc::new(WebCapability("web_search")));
        agent.begin_tool_routing("Fix the arithmetic bug in calc.py");
        let offered = agent.offered();
        let mut broad = offered.clone();
        assert!(agent.catalog().iter().any(|tool| tool == "web_search"));
        broad.push("web_search".into());
        broad.sort();
        broad.dedup();
        let before = schema_bytes(agent, &broad);
        let after = schema_bytes(agent, &offered);
        eprintln!("coding tool schemas without unrelated web tools: {before} -> {after} bytes");
        assert!(
            after < before,
            "unrelated web schemas still offered: {offered:?}"
        );
        assert!(
            agent
                .discoverable_tools(&offered)
                .iter()
                .any(|(name, _)| name == "web_search")
        );
        agent.begin_tool_routing("Research this issue online");
        for name in ["read", "web_search"] {
            assert!(agent.offered().iter().any(|tool| tool == name));
        }
        assert!(
            agent.enabled_tools.is_empty(),
            "routing must not require a model discovery call"
        );
        agent.activate_tools(&["web_search".into()]).unwrap();
        agent.begin_tool_routing("Fix the arithmetic bug in calc.py");
        assert!(agent.offered().iter().any(|tool| tool == "web_search"));
    });
}

struct WebCapability(&'static str);
impl crate::tools::integrations::ext::Tool for WebCapability {
    fn name(&self) -> &str {
        self.0
    }
    fn schema(&self) -> Value {
        json!({"type":"function","function":{"name":self.0,"description":"Web research capability", "parameters":{"type":"object","properties":{"query":{"type":"string"}}}}})
    }
    fn needs_approval(&self, _: &Value) -> bool {
        true
    }
    fn call(&self, _: &crate::tools::integrations::ext::ToolContext, _: &Value) -> ToolOutput {
        panic!("schema measurement must not execute a tool")
    }
}

#[test]
fn offered_native_schemas_are_lean_and_argument_help_restores_explanations() {
    with_agent("", |agent| {
        let names: Vec<String> = [
            "read", "write", "edit", "glob", "grep", "bash", "bg", "eval", "todo", "verify", "task",
        ]
        .into_iter()
        .map(str::to_owned)
        .filter(|name| agent.full_schemas(std::slice::from_ref(name)).is_ok())
        .collect();
        let bare = serde_json::to_string(&agent.schemas(&names).unwrap()).unwrap();
        let full = serde_json::to_string(&agent.full_schemas(&names).unwrap()).unwrap();
        eprintln!(
            "core tool schemas: {} bytes full, {} bytes bare",
            full.len(),
            bare.len()
        );
        assert!(bare.len() < full.len());
        // Core and multi-action tools keep short top-level explanations; the rest are bare.
        for schema in agent.schemas(&names).unwrap() {
            let name = schema["function"]["name"].as_str().unwrap();
            let parameters = &schema["function"]["parameters"];
            for property in parameters["properties"].as_object().unwrap().values() {
                let nested = property
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(key, _)| *key != "description");
                assert!(
                    nested
                        .into_iter()
                        .all(|(_, value)| !value.to_string().contains("\"description\"")),
                    "{schema}"
                );
                if let Some(text) = property["description"].as_str() {
                    assert!(super::DESCRIBED_PARAMETER_TOOLS.contains(&name), "{schema}");
                    assert!(text.split_whitespace().count() <= super::PARAMETER_DESCRIPTION_WORDS);
                    let encoded = json!(text).to_string();
                    assert!(full.contains(encoded.trim_matches('"')), "{text}");
                }
            }
            if !super::DESCRIBED_PARAMETER_TOOLS.contains(&name) {
                assert!(
                    !parameters.to_string().contains("\"description\""),
                    "{schema}"
                );
            }
        }
        // Nothing is hidden: the bare grep schema still offers every parameter.
        let grep = &agent.schemas(&["grep".to_owned()]).unwrap()[0];
        assert!(grep["function"]["parameters"]["properties"]["cursor"].is_object());
        let help = agent.parameter_help("grep").unwrap();
        assert!(
            help.contains("- pattern (string, required): Non-empty Rust regex"),
            "{help}"
        );
    });
}

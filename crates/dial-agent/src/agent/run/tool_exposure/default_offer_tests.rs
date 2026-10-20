use super::*;
use std::sync::atomic::AtomicBool;

/// An interactive (non-hermetic) agent over default configuration plus `overlay` and an
/// unreachable endpoint; these tests inspect the offer and prompt only, so no request is made.
pub(crate) fn with_agent<T>(overlay: &str, f: impl FnOnce(&mut Agent<'_>) -> T) -> T {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    let mut overlay: toml::Table =
        toml::from_str(&format!("{overlay}\n[improve]\nenabled=false")).unwrap();
    overlay.entry("model").or_insert_with(|| "test".into());
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
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(prompt.contains("Descendants require explicit role spawn permissions"));
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
            "recall",
            "retain",
            "codebase_memory",
            "tool_load",
            "tool_search",
        ] {
            assert!(
                offered.iter().any(|name| name == core),
                "{core}: {offered:?}"
            );
        }
        let discoverable = agent.discoverable_tools(&offered);
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(prompt.contains("Loadable tools, not in the schema yet"));
        for deferred in tools::DEFERRED_BY_DEFAULT {
            assert!(catalog.iter().any(|name| name == deferred), "{deferred}");
            assert!(!offered.iter().any(|name| name == deferred), "{offered:?}");
            assert!(
                discoverable.iter().any(|(name, _)| name == deferred),
                "{discoverable:?}"
            );
            assert!(prompt.contains(&format!("  - {deferred}: ")), "{prompt}");
        }
        // Serialized default schema size, before (deferred tools offered) and after.
        let mut before = offered.clone();
        before.extend(
            tools::DEFERRED_BY_DEFAULT
                .iter()
                .map(|name| (*name).to_owned()),
        );
        before.sort();
        let (before, after) = (schema_bytes(agent, &before), schema_bytes(agent, &offered));
        eprintln!("default tool schemas: {before} -> {after} bytes");
        assert!(after < before);
    });
}

#[test]
fn tool_load_offers_a_deferred_tool_next_round_and_unload_defers_it_again() {
    with_agent("", |agent| {
        struct Quiet;
        impl Ui for Quiet {}
        let mut backend = agent.backend.take().unwrap();
        let mut call = |agent: &mut Agent<'_>, arguments: Value| {
            let call = ToolCall {
                id: "call-load".into(),
                name: "tool_load".into(),
                arguments,
                malformed_arguments: false,
            };
            agent.dispatch(&mut backend, &call, &mut Quiet)
        };
        let names: Vec<&str> = tools::DEFERRED_BY_DEFAULT.to_vec();
        let out = call(agent, json!({"names": names}));
        assert!(!out.is_error, "{}", out.text);
        let offered = agent.offered();
        for deferred in tools::DEFERRED_BY_DEFAULT {
            assert!(offered.iter().any(|name| name == deferred), "{offered:?}");
        }
        let prompt = serde_json::to_string(&agent.system_messages()).unwrap();
        assert!(!prompt.contains("  - memory_update: "), "{prompt}");
        let out = call(agent, json!({"unload": ["memory_update"]}));
        assert!(!out.is_error, "{}", out.text);
        let offered = agent.offered();
        assert!(!offered.iter().any(|name| name == "memory_update"));
        assert!(offered.iter().any(|name| name == "ast_search"));
    });
}

#[test]
fn pinned_harness_selection_keeps_deferred_tools_offered() {
    with_agent(
        "[harness]\ntools=['read','edit','bash','recall','retain','memory_update']\n[assistance]\nlean_local=false",
        |agent| {
            let offered = agent.offered();
            assert!(
                offered.iter().any(|name| name == "memory_update"),
                "{offered:?}"
            );
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
            let offered = agent.offered();
            for tool in ["recall", "retain"] {
                assert!(
                    offered.iter().any(|name| name == tool),
                    "{profile}: {offered:?}"
                );
            }
            // The fallback used when the full set does not fit the window keeps them too.
            agent.lean_tools = true;
            let offered = agent.offered();
            for tool in ["recall", "retain"] {
                assert!(
                    offered.iter().any(|name| name == tool),
                    "{profile}: {offered:?}"
                );
            }
            // Corrections stay loadable, like provenance's.
            assert!(!offered.iter().any(|name| name == "forget"));
            assert!(agent.catalog().iter().any(|name| name == "forget"));
            agent.schemas(&offered).unwrap();
        });
    }
    // Provenance's own recall/retain are protected the same way.
    with_agent("[assistance]\ncompact_tools=true", |agent| {
        agent.lean_tools = true;
        let offered = agent.offered();
        for tool in ["recall", "retain"] {
            assert!(offered.iter().any(|name| name == tool), "{offered:?}");
        }
    });
}

#[test]
fn autolearn_offers_learn_up_front_and_guides_the_root_agent() {
    with_agent(
        "[memory]\nbackend='local'\nautolearn=true\n[assistance]\nlean_local=false",
        |agent| {
            let offered = agent.offered();
            assert!(offered.iter().any(|name| name == "learn"), "{offered:?}");
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
        assert!(system_text(agent).contains("`recall` searches facts"));
    });
    with_agent("[memory]\nbackend='semantic'", |agent| {
        assert!(system_text(agent).contains("`retain` saves a durable fact"));
    });
    with_agent("[memory]\nbackend='sharpshooter'", |agent| {
        let text = system_text(agent);
        assert!(text.contains("saved project decisions"), "{text}");
        assert!(!text.contains("`retain` saves"));
    });
    for off in ["[memory]\nbackend='off'", "[memory]\nbackend='local'"] {
        with_agent(off, |agent| {
            assert!(!system_text(agent).contains("Memory: `recall`"), "{off}");
        });
    }
}

#[test]
fn discovered_working_set_is_bounded_persisted_and_separate_from_pins() {
    with_agent("[assistance]\ncompact_tools=true", |agent| {
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
    });
}

#[test]
fn compact_discovery_pins_and_unloads_do_not_leave_hidden_activation_state() {
    with_agent("[assistance]\ncompact_tools=true", |agent| {
        struct Quiet;
        impl Ui for Quiet {}
        let name = "ast_search".to_owned();
        agent.activate_tools(std::slice::from_ref(&name)).unwrap();
        let mut backend = agent.backend.take().unwrap();
        for arguments in [json!({"names":[name]}), json!({"unload":[name]})] {
            let call = ToolCall {
                id: "pin-discovered".into(),
                name: "tool_load".into(),
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
    });
}

#[test]
fn default_stale_reduction_precedes_automatic_summary_pressure() {
    with_agent("context_window=128000", |agent| {
        let mut messages = vec![
            json!({"role":"user","content":"task"}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"old","content":"x".repeat(200_000),"dial_evidence":{"artifact":"artifact://old","observation":"same-range"}}),
            json!({"role":"assistant","content":"","tool_calls":[{"id":"new","function":{"name":"read","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"new","content":"y".repeat(160_000),"dial_evidence":{"artifact":"artifact://new","observation":"same-range"}}),
        ];
        let raw = crate::llm::wire::estimate_parts(&messages, &[]);
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
        assert!(raw > threshold);
        assert!(raw < u64::from(agent.budget_window()) * 75 / 100);
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
        offered.iter().all(|name| {
            Agent::LEAN_TOOLS.contains(&name.as_str())
                || matches!(name.as_str(), "recall" | "retain")
        })
    };
    with_agent("context_window=16384", |agent| {
        assert_eq!(agent.local_tier(), Some(Tier::Small));
        let offered = agent.offered();
        assert!(minimal(&offered), "{offered:?}");
        assert!(offered.iter().any(|name| name == "edit"), "{offered:?}");
        // Every other tool stays reachable through the loaders.
        assert!(
            offered.iter().any(|name| name == "tool_load"),
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
/// replace grammar. A success resets the streak, the step is remembered for the model and a
/// new session starts from it.
#[test]
fn local_escalation_steps_up_on_failing_rounds_and_is_remembered() {
    use crate::config::types::EditMode;
    let overlay = "context_window=65536\n[assistance.local]\nedit_mode='inherit'";
    with_agent(overlay, |agent| {
        let ui = &mut crate::agent::NullUi;
        let full = agent.offered();
        assert!(!agent.minimal_tools());
        assert_eq!(agent.edit_mode(), EditMode::Hashline);
        assert!(!agent.observe_local_round(true, ui));
        assert!(!agent.observe_local_round(false, ui), "a success resets");
        assert!(!agent.observe_local_round(true, ui));
        assert!(agent.observe_local_round(true, ui));
        assert!(agent.minimal_tools());
        assert!(agent.offered().len() < full.len());
        assert!(!system_text(agent).contains("# Visual verification"));
        assert_eq!(agent.edit_mode(), EditMode::Hashline);
        assert!(!agent.observe_local_round(true, ui));
        assert!(agent.observe_local_round(true, ui));
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
    assert_eq!(apply(local, 0.7), (None, None));
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
    assert_eq!(cap("context_window=16384"), Some(16384 * 4 / 2));
    assert_eq!(cap("context_window=32768"), Some(32768 * 4 / 2));
    assert_eq!(cap("context_window=65536"), Some(65536 * 4 / 2));
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
                    agent.modes.launch.tools = Some(
                        [
                            "read".to_owned(),
                            "tool_load".to_owned(),
                            "tool_search".to_owned(),
                        ]
                        .into(),
                    )
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
                    name: "tool_load".into(),
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
fn only_small_mcp_servers_are_offered_up_front() {
    let mut catalog: Vec<String> = vec!["read".into()];
    catalog.extend((0..8).map(|i| format!("mcp__big__t{i}")));
    catalog.extend(["mcp__small__a".into(), "mcp__small__b".into()]);
    assert_eq!(
        Agent::small_mcp_tools(&catalog),
        ["mcp__small__a", "mcp__small__b"]
    );

    let many: Vec<String> = (0..10)
        .flat_map(|s| (0..4).map(move |t| format!("mcp__s{s:02}__t{t}")))
        .collect();
    assert!(Agent::small_mcp_tools(&many).len() <= Agent::AUTO_MCP_TOOL_LIMIT);
}

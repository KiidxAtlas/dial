use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

/// Phase thinking: a successful tool round makes the next step mechanical; a failure or a plan
/// change thinks again.
#[test]
fn local_phase_thinking_follows_round_outcomes() {
    with_agent("[assistance.local]\nthinking='phase'", |agent| {
        let ui = &mut crate::agent::NullUi;
        agent.observe_local_round(false, ui);
        assert!(!agent.local_session.think_next);
        agent.observe_local_round(true, ui);
        assert!(agent.local_session.think_next);
        agent.observe_local_plan_change();
        agent.observe_local_round(false, ui);
        assert!(agent.local_session.think_next);
        agent.observe_local_round(false, ui);
        assert!(!agent.local_session.think_next, "a plan change counts once");
    });
}

/// Counters reach `local-profile.json` only with `assistance.local.suggestions` on; the
/// escalation step is kept either way.
#[test]
fn local_evidence_counters_follow_the_suggestions_switch() {
    let flushed = |overlay: &str| {
        with_agent(overlay, |agent| {
            let ui = &mut crate::agent::NullUi;
            agent.observe_local_round(true, ui);
            agent.observe_local_edit(false);
            agent.flush_local_evidence().unwrap();
            crate::llm::local_evidence::read(&agent.config.user_dir)
                .unwrap()
                .remove(&agent.local_model_key())
        })
    };
    let on = flushed("").expect("evidence recorded");
    assert_eq!((on.rounds.ok, on.rounds.failed), (0, 1));
    assert_eq!(on.edits.get("replace").map(|t| t.failed), Some(1));
    assert_eq!(flushed("[assistance.local]\nsuggestions=false"), None);
    let staged = flushed("[assistance.local]\nsuggestions=false\nescalate_after=1")
        .expect("the escalation step is kept");
    assert_eq!(staged.stage, 1);
    assert_eq!(staged.rounds, Default::default());
    assert!(staged.edits.is_empty());
}

/// A side role on another model of the main model's local server swaps weights; `reuse_main`
/// keeps side requests on the main model, hosted side roles are never affected.
#[test]
fn local_side_models_detect_swaps_on_the_main_server() {
    let local_small = "[model_roles.small]\nprovider='mlx'\nmodel='other'";
    with_agent(local_small, |agent| {
        assert!(agent.local_side_swap("small"));
        assert!(
            !agent.local_side_swap("coach"),
            "unassigned roles use the main model"
        );
        assert!(
            !agent.local_side_reuses_main("small"),
            "warn is the default"
        );
    });
    with_agent(
        &format!("{local_small}\n[assistance.local]\nside_models='reuse_main'"),
        |agent| assert!(agent.local_side_reuses_main("small")),
    );
    with_agent(
        "[model_roles.small]\nprovider='mlx'\nmodel='test'",
        |agent| {
            assert!(!agent.local_side_swap("small"), "same model, no swap");
        },
    );
    with_agent(
        "[model_roles.small]\nprovider='anthropic'\nmodel='claude-x'\n[assistance.local]\nside_models='reuse_main'",
        |agent| assert!(!agent.local_side_reuses_main("small")),
    );
}

#[test]
fn workers_and_side_requests_avoid_the_main_slot() {
    assert_eq!(worker_slot(None, 0), None);
    assert_eq!(worker_slot(Some(1), 3), None);
    assert_eq!(worker_slot(Some(2), 0), Some(1));
    assert_eq!(worker_slot(Some(2), 5), Some(1));
    assert_eq!(worker_slot(Some(4), 0), Some(1));
    assert_eq!(worker_slot(Some(4), 2), Some(3));
    assert_eq!(worker_slot(Some(4), 3), Some(1));
    assert_eq!(side_slot(None), None);
    assert_eq!(side_slot(Some(1)), None);
    assert_eq!(side_slot(Some(4)), Some(3));
    for slots in 2..6 {
        for worker in 0..10 {
            assert_ne!(worker_slot(Some(slots), worker), Some(MAIN_SLOT));
        }
    }
}

#[test]
fn time_budget_follows_measured_speed_within_bounds() {
    // 4000 prompt tokens at 400 t/s (10 s) and 1000 output tokens at 10 t/s (100 s).
    assert_eq!(time_budget_secs(4000, 1000, Some(400.0), 10.0), 225);
    // Unknown prefill speed counts generation only; a fast model still gets two minutes.
    assert_eq!(time_budget_secs(4000, 1000, None, 10.0), 210);
    assert_eq!(time_budget_secs(100, 100, Some(5000.0), 200.0), 120);
    assert_eq!(time_budget_secs(0, 1_000_000, None, 1.0), 3600);
    assert_eq!(time_budget_secs(4000, 1000, Some(f64::NAN), 0.0), 3600);
}

#[test]
fn local_thinking_defaults_leave_model_settings_unchanged_across_rounds() {
    assert_eq!(LocalThinking::default(), LocalThinking::Model);
    for overlay in ["", "[assistance.local]\nthinking='model'"] {
        with_agent(overlay, |agent| {
            assert_eq!(agent.config.assistance.local.thinking, LocalThinking::Model);
            agent.local_session.probed = true;
            agent.local_session.facts.thinking_switch = Some(true);
            let mut backend = agent.backend.take().unwrap();
            let mut profile = None;
            for failed in [false, true, false] {
                agent.observe_local_round(failed, &mut crate::agent::NullUi);
                let controls = agent
                    .prepare_local_request(&mut backend, &mut profile)
                    .unwrap();
                assert!(controls["thinking"].is_null(), "{controls}");
            }
            agent.observe_local_plan_change();
            agent.observe_local_round(false, &mut crate::agent::NullUi);
            let controls = agent
                .prepare_local_request(&mut backend, &mut profile)
                .unwrap();
            assert!(controls["thinking"].is_null(), "{controls}");
        });
    }
}

#[test]
fn explicit_phase_thinking_still_sets_per_request_controls() {
    with_agent("[assistance.local]\nthinking='phase'", |agent| {
        agent.local_session.probed = true;
        agent.local_session.facts.thinking_switch = Some(true);
        let mut backend = agent.backend.take().unwrap();
        let mut profile = None;
        for failed in [false, true, false] {
            agent.observe_local_round(failed, &mut crate::agent::NullUi);
            let controls = agent
                .prepare_local_request(&mut backend, &mut profile)
                .unwrap();
            assert_eq!(controls["thinking"], json!(failed));
        }
    });
}

fn prompt_text(agent: &crate::agent::Agent<'_>) -> String {
    serde_json::to_string(&agent.system_messages()).unwrap()
}

/// The small tier defaults to one call per response, stated by a byte-stable prompt that ends with
/// a worked example in the replace grammar and names no unoffered `skill`. A saved record that
/// passed the batch fixture keeps batching; larger tiers, a disabled reliability interface and
/// hosted routes are unchanged.
#[test]
fn small_tier_defaults_to_one_call_with_a_worked_example() {
    with_agent("context_window=16384", |agent| {
        assert_eq!(agent.local_tier(), Some(Tier::Small));
        assert!(agent.local_one_call());
        let text = prompt_text(agent);
        let rule = serde_json::to_string(crate::agent::prompts::system::LEAN_ONE_CALL).unwrap();
        assert!(text.contains(rule.trim_matches('"')));
        assert!(!text.contains("MUST batch"));
        assert!(text.contains("# Example"));
        assert!(
            text.contains(r#"\"old_string\":\"    total / size\""#),
            "{text}"
        );
        assert!(!text.contains(crate::agent::prompts::system::SKILL_RULE));
        agent.rebuild_system();
        assert_eq!(prompt_text(agent), text, "byte-stable");
    });
    with_agent("context_window=16384", |agent| {
        agent.local_session.turn_record = Some(json!({"batch_passed": true}));
        agent.rebuild_system();
        assert!(!agent.local_one_call());
        let text = prompt_text(agent);
        assert!(text.contains("MUST batch"));
        assert!(
            !text.contains("# Example"),
            "passing batch evidence omits the single-call tutorial"
        );
    });
    for overlay in [
        "context_window=65536",
        "context_window=16384\n[reliability]\nenabled=false",
        "context_window=16384\n[endpoint]\nbase_url='https://api.example.com/v1'",
    ] {
        with_agent(overlay, |agent| {
            assert!(!agent.local_one_call(), "{overlay}");
            assert!(!prompt_text(agent).contains("# Example"), "{overlay}");
            assert!(
                !prompt_text(agent).contains(
                    serde_json::to_string(crate::agent::prompts::system::LEAN_ONE_CALL)
                        .unwrap()
                        .trim_matches('"')
                ),
                "{overlay}"
            );
        });
    }
    with_agent("context_window=65536", |agent| {
        assert!(!prompt_text(agent).contains("# Example"));
    });
}

/// The small and lean local tiers make the edit tool refuse new parser errors; the full tier and
/// hosted routes keep the advisory, as does `assistance.syntax_guard = false`.
#[test]
fn lean_local_tiers_block_syntax_regressions_in_edits() {
    for (overlay, blocking) in [
        ("context_window=16384", true),
        ("context_window=65536", true),
        (
            "context_window=16384\n[assistance]\nlean_local=false",
            false,
        ),
        (
            "context_window=16384\n[assistance]\nsyntax_guard=false",
            false,
        ),
        (
            "context_window=16384\n[endpoint]\nbase_url='https://api.example.com/v1'",
            false,
        ),
    ] {
        with_agent(overlay, |agent| {
            assert_eq!(agent.workspace.syntax_blocking(), blocking, "{overlay}");
        });
    }
}

/// A llama.cpp server whose chat template cannot render tools starts the session at the minimal
/// tool set; one that can, an older build without capabilities, `escalate_after = 0` and the
/// evidence file are all unaffected.
#[test]
fn a_template_without_native_tools_starts_at_the_minimal_tool_set() {
    use crate::llm::fake;
    let props = |caps: serde_json::Value| {
        let mut body = serde_json::json!({"total_slots": 1, "default_generation_settings": {}});
        if !caps.is_null() {
            body["chat_template_caps"] = caps;
        }
        let server = fake::serve(vec![fake::json(200, &body)]);
        let url = server.url.clone();
        (server, format!("[endpoint]\nbase_url='{url}/v1'"))
    };
    let started = |overlay: &str| {
        with_agent(overlay, |agent| {
            agent.seed_local_escalation();
            let stage = agent.local_escalation.stage;
            let recorded = crate::llm::local_evidence::read(&agent.config.user_dir)
                .unwrap()
                .remove(&agent.local_model_key());
            (stage, agent.minimal_tools(), recorded)
        })
    };
    let (_server, overlay) =
        props(serde_json::json!({"supports_tools": false, "supports_tool_calls": false}));
    let (stage, minimal, recorded) = started(&format!("context_window=65536\n{overlay}"));
    assert_eq!((stage, minimal), (1, true));
    assert!(recorded.is_none(), "a server fact is not saved as evidence");

    let (_server, overlay) =
        props(serde_json::json!({"supports_tools": true, "supports_tool_calls": true}));
    assert_eq!(started(&format!("context_window=65536\n{overlay}")).0, 0);
    let (_server, overlay) = props(serde_json::Value::Null);
    assert_eq!(started(&format!("context_window=65536\n{overlay}")).0, 0);
    let (_server, overlay) =
        props(serde_json::json!({"supports_tools": false, "supports_tool_calls": false}));
    let off = format!("context_window=65536\n[assistance.local]\nescalate_after=0\n{overlay}");
    assert_eq!(started(&off).0, 0);

    // LM Studio: a model whose capability list lacks `tool_use` starts at the minimal set.
    let studio = |capabilities: &str| {
        let body = serde_json::json!({"data":[{"id":"test",
            "capabilities": serde_json::from_str::<serde_json::Value>(capabilities).unwrap()}]});
        let server = fake::serve(vec![fake::json(200, &body)]);
        started(&format!(
            "context_window=65536\nprovider='lm-studio'\n[endpoint]\nbase_url='{}/v1'",
            server.url
        ))
        .0
    };
    assert_eq!(studio("[]"), 1);
    assert_eq!(studio(r#"["tool_use"]"#), 0);
}

/// Repeated replies whose tool calls Dial read out of text produce one notice naming the fix for
/// the detected server; a single lift, structured calls and hosted routes produce none.
#[test]
fn repeated_text_calls_advise_the_servers_tool_parser_once() {
    #[derive(Default)]
    struct Notices(Vec<String>);
    impl crate::agent::Ui for Notices {
        fn notice(&mut self, message: &str) {
            self.0.push(message.to_owned());
        }
    }
    let lifted = |n: usize, server: Option<LocalServer>, overlay: &str| {
        with_agent(overlay, |agent| {
            agent.local_session.server = server;
            let mut backend = agent.backend.take().unwrap();
            for lifted in std::iter::repeat_n(true, n) {
                let reply = crate::llm::Reply {
                    text_calls_lifted: lifted,
                    ..Default::default()
                };
                agent.observe_local_reply(&reply, &mut backend);
            }
            agent.backend = Some(backend);
            let mut ui = Notices::default();
            for _ in 0..3 {
                agent.observe_local_round(false, &mut ui);
            }
            ui.0
        })
    };
    assert!(lifted(1, None, "").is_empty());
    let notices = lifted(2, Some(LocalServer::Vllm), "");
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("--tool-call-parser") && notices[0].contains("2 times"));
    assert!(lifted(2, Some(LocalServer::LlamaCpp), "")[0].contains("--jinja"));
    let hosted = "[endpoint]\nbase_url='https://api.example.com/v1'";
    assert!(lifted(3, None, hosted).is_empty());
}

/// Writes a saved record for this agent's exact binding, as `dial reliability check` would.
fn write_record(agent: &Agent<'_>, record: serde_json::Value) {
    crate::platform::atomic::private_dir(&agent.config.state_dir).unwrap();
    let mut data = json!({"version":1,"measured_at":crate::learn::improve::now(),"binding":crate::reliability::binding(agent.config)});
    data.as_object_mut()
        .unwrap()
        .extend(record.as_object().unwrap().clone());
    std::fs::write(
        agent.config.state_dir.join("reliability.json"),
        serde_json::to_vec(&data).unwrap(),
    )
    .unwrap();
}
/// Overlay for the large-window cases: 65536 is above the small tier's 1024 limit.
const LARGE: &str = "context_window=65536\n[assistance.local]\nsmall_window=1024";

/// A saved fixture case the model could not call seeds the minimal step on a large window; the edit
/// grammar and its pin are kept, including at the second step.
#[test]
fn measured_fixture_failure_seeds_minimal_step_at_a_large_window() {
    with_agent(
        &format!("{LARGE}\n[edit]\nmode='hashline'\n[reliability]\nmeasured_interfaces=true"),
        |agent| {
            assert_eq!(agent.local_tier(), Some(Tier::Lean));
            write_record(
                agent,
                json!({"batch_passed":true,"passed":4,"total":5,"cases":[{"tool":"edit","passed":false}]}),
            );
            agent.seed_local_escalation();
            assert!(agent.local_escalated());
            assert!(agent.minimal_tools());
            assert_eq!(
                agent.escalated_edit_mode(),
                None,
                "the second step needs a measured alternative"
            );
            agent.local_escalation.stage = 2;
            assert_eq!(
                agent.escalated_edit_mode(),
                None,
                "a pin decides the grammar at the second step"
            );
            assert_eq!(agent.edit_mode(), crate::config::types::EditMode::Hashline);
        },
    );
    with_agent(
        &format!("{LARGE}\n[reliability]\nmeasured_interfaces=true"),
        |agent| {
            write_record(
                agent,
                json!({"batch_passed":true,"passed":4,"total":5,"cases":[{"tool":"edit","passed":false}]}),
            );
            agent.seed_local_escalation();
            agent.local_escalation.stage = 2;
            assert_eq!(
                agent.escalated_edit_mode(),
                Some(crate::config::types::EditMode::Replace)
            );
        },
    );
}

/// The seed follows `reliability.enabled` and `measured_interfaces`: with either off, a saved failing
/// fixture leaves the step where it was.
#[test]
fn fixture_seed_respects_disabled_reliability_interfaces() {
    for overlay in [
        format!("{LARGE}\n[reliability]\nenabled=false\nmeasured_interfaces=true"),
        format!("{LARGE}\n[reliability]\nmeasured_interfaces=false"),
    ] {
        with_agent(&overlay, |agent| {
            write_record(
                agent,
                json!({"batch_passed":false,"passed":0,"total":5,"cases":[{"tool":"edit","passed":false}]}),
            );
            agent.seed_local_escalation();
            assert!(!agent.local_escalated(), "{overlay}");
        });
    }
}

/// An inactive profile leaves the once-per-session seed unconsumed.
#[test]
fn inactive_profile_does_not_consume_the_seed() {
    with_agent(
        "[endpoint]\nbase_url='https://api.example.com/v1'",
        |agent| {
            agent.seed_local_escalation();
            assert!(!agent.local_escalation.seeded);
        },
    );
}

/// A passing record leaves the large-window route on the lean offer.
#[test]
fn passing_fixture_record_does_not_seed_the_minimal_step() {
    with_agent(
        &format!("{LARGE}\n[reliability]\nmeasured_interfaces=true"),
        |agent| {
            write_record(
                agent,
                json!({"batch_passed":true,"passed":5,"total":5,"cases":[{"tool":"edit","passed":true}]}),
            );
            agent.seed_local_escalation();
            assert!(!agent.local_escalated());
        },
    );
}

/// The batch verdict follows the record at each turn start, never mid-turn.
#[test]
fn replaced_record_changes_the_batch_verdict_at_the_next_turn_start() {
    with_agent("context_window=16384", |agent| {
        let rule = serde_json::to_string(crate::agent::prompts::system::LEAN_ONE_CALL).unwrap();
        write_record(agent, json!({"batch_passed":true,"passed":5,"total":5}));
        agent.refresh_turn_record();
        agent.rebuild_system();
        assert!(!agent.local_one_call());
        write_record(agent, json!({"batch_passed":false,"passed":2,"total":5}));
        assert!(
            !agent.local_one_call(),
            "a mid-turn replacement waits for the next turn"
        );
        agent.refresh_turn_record();
        agent.rebuild_system();
        assert!(agent.local_one_call());
        assert!(prompt_text(agent).contains(rule.trim_matches('"')));
    });
}

#[test]
fn base_fixture_record_is_not_reused_for_adapted_or_hermetic_turns() {
    for adapted in [true, false] {
        with_agent("context_window=16384", |agent| {
            write_record(agent, json!({"batch_passed":true,"passed":5,"total":5}));
            assert!(crate::reliability::measured(agent.config).is_some());
            if adapted {
                agent.version.adapter = Some("unloaded-test-adapter".into());
            } else {
                agent.hermetic = true;
            }
            agent.refresh_turn_record();
            assert!(agent.take_turn_record().is_none());
            assert!(agent.local_session.turn_record.is_none());
            if adapted {
                assert!(
                    agent.local_one_call(),
                    "the unmeasured small tier keeps its default"
                );
            }
        });
    }
}

/// A read-only success between failed edits does not reset the streak while the latest edit or
/// write is unresolved, so the escalation threshold is still reached. A successful edit resolves
/// the failure, and the streak then starts over.
#[test]
fn a_read_between_failed_edits_keeps_the_escalation_streak() {
    let overlay = "context_window=65536\n[assistance.local]\nedit_mode='inherit'";
    with_agent(overlay, |agent| {
        let ui = &mut crate::agent::NullUi;
        agent.observe_local_edit(false);
        assert!(!agent.observe_local_round(true, ui));
        assert!(
            !agent.observe_local_round(false, ui),
            "a read is not a recovery"
        );
        agent.observe_local_edit(false);
        assert!(
            agent.observe_local_round(true, ui),
            "failed, read, failed reaches the step"
        );
    });
    // The unresolved failure carries across the turn boundary as well.
    with_agent(overlay, |agent| {
        let ui = &mut crate::agent::NullUi;
        agent.seed_local_escalation();
        agent.observe_local_edit(false);
        assert!(!agent.observe_local_round(true, ui));
        agent.seed_local_escalation();
        assert!(!agent.observe_local_round(false, ui));
        agent.observe_local_edit(false);
        assert!(agent.observe_local_round(true, ui));
    });
    // A later successful edit resolves the failure, so a read followed by one new failure is
    // only the first strike.
    with_agent(overlay, |agent| {
        let ui = &mut crate::agent::NullUi;
        agent.observe_local_edit(false);
        assert!(!agent.observe_local_round(true, ui));
        agent.observe_local_edit(true);
        assert!(!agent.observe_local_round(false, ui));
        agent.observe_local_edit(false);
        assert!(
            !agent.observe_local_round(true, ui),
            "the earlier failure was resolved"
        );
    });
}

/// Reads that are not preceded by an unresolved edit failure still reset, as before.
#[test]
fn a_read_without_an_unresolved_edit_failure_still_resets_the_streak() {
    with_agent("context_window=65536", |agent| {
        let ui = &mut crate::agent::NullUi;
        assert!(!agent.observe_local_round(true, ui));
        assert!(!agent.observe_local_round(false, ui));
        assert!(!agent.observe_local_round(true, ui));
        assert!(
            agent.observe_local_round(true, ui),
            "the reset left one strike, so this is the second"
        );
    });
}

#[test]
fn edit_failure_streak_expires_after_two_successful_rounds() {
    with_agent("context_window=65536", |agent| {
        let ui = &mut crate::agent::NullUi;
        agent.observe_local_edit(false);
        assert!(!agent.observe_local_round(true, ui));
        assert!(!agent.observe_local_round(false, ui));
        assert!(!agent.observe_local_round(false, ui));
        agent.observe_local_edit(false);
        assert!(!agent.observe_local_round(true, ui), "old failure expired");
        assert!(agent.observe_local_round(true, ui));
    });
}

#[test]
fn mixed_round_keeps_failure_but_later_success_clears_streak() {
    with_agent("context_window=65536", |agent| {
        let ui = &mut crate::agent::NullUi;
        agent.observe_local_edit(false);
        agent.observe_local_edit(true);
        assert!(
            !agent.observe_local_round(true, ui),
            "round contained a failure"
        );
        assert!(!agent.observe_local_round(false, ui));
        agent.observe_local_edit(false);
        assert!(
            !agent.observe_local_round(true, ui),
            "success cleared prior streak"
        );
    });
}

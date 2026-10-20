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
        assert!(text.contains("Make exactly one tool call per response"));
        assert!(!text.contains("MUST batch"));
        assert!(text.contains("# Example"));
        assert!(
            text.contains(r#"\"old_string\":\"    total / size\""#),
            "{text}"
        );
        assert!(!text.contains("MUST load it with `skill` first"));
        agent.rebuild_system();
        assert_eq!(prompt_text(agent), text, "byte-stable");
    });
    with_agent("context_window=16384", |agent| {
        agent.local_session.batch_record_passed = std::cell::OnceCell::from(true);
        agent.rebuild_system();
        assert!(!agent.local_one_call());
        let text = prompt_text(agent);
        assert!(text.contains("MUST batch"));
        assert!(text.contains("# Example"));
    });
    for overlay in [
        "context_window=65536",
        "context_window=16384\n[reliability]\nenabled=false",
        "context_window=16384\n[endpoint]\nbase_url='https://api.example.com/v1'",
    ] {
        with_agent(overlay, |agent| {
            assert!(!agent.local_one_call(), "{overlay}");
            assert!(
                !prompt_text(agent).contains("Make exactly one tool call"),
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

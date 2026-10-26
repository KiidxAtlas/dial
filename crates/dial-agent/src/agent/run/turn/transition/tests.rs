//! The reset table: what each transition clears and what survives it.
use super::super::completion_tests::connect;
use super::*;
use crate::agent::run::guidance::{Priority, note};
use crate::agent::run::tool_exposure::default_offer_tests::with_agent;

/// Mark every instruction-scoped and turn-wide field as used, so a reset is observable.
fn worn(turn: &mut TurnState<'_>) {
    turn.auto_continues = 2;
    turn.stall_continues = 1;
    turn.overflow_retries = 1;
    turn.solo_todo_noted = true;
    turn.automatic_coach_pending = true;
    turn.planning_notice = true;
    turn.rejected_edit_previews = 3;
    turn.compaction_failed = true;
    turn.rule_regenerations = 1;
    turn.unpriced_cap_noted = true;
    turn.reviewed_paths.insert("src/lib.rs".into());
    turn.completion_candidate = Some("done".into());
    turn.reliability.consecutive_contract_failures = 2;
    turn.pending_nudges = vec![
        note(Priority::Failure, "old failure advice"),
        note(Priority::Safety, "safety boundary"),
    ];
}

fn with_turn(check: impl Fn(&mut TurnState<'_>)) {
    with_agent("[compaction]\nenabled=false\n", |agent| {
        connect(agent, "http://127.0.0.1:9");
        let mut backend = agent.backend.take().unwrap();
        let names = agent.offered();
        let mut turn = agent
            .begin_turn(&mut backend, &names, 0, "goal", &mut NullUi)
            .unwrap();
        worn(&mut turn);
        check(&mut turn);
    });
}

#[test]
fn steering_starts_the_instruction_over_but_keeps_turn_bounds_and_project_facts() {
    with_turn(|turn| {
        let epoch = turn.enrichment_epoch;
        turn.reset(Transition::Steering);
        // The new instruction gets its own recovery rounds, advice and reviews.
        assert_eq!(turn.auto_continues, 0);
        assert_eq!(turn.stall_continues, 0);
        assert_eq!(turn.overflow_retries, 0);
        assert!(!turn.solo_todo_noted);
        assert!(!turn.automatic_coach_pending);
        assert!(!turn.planning_notice);
        assert_eq!(turn.rejected_edit_previews, 0);
        assert!(!turn.compaction_failed);
        assert_eq!(turn.completion_candidate, None);
        assert_eq!(turn.reliability.consecutive_contract_failures, 0);
        assert_eq!(turn.enrichment_epoch, epoch.wrapping_add(1));
        // Only safety boundaries outlive the old task's advice.
        assert_eq!(turn.pending_nudges.len(), 1);
        assert!(
            turn.pending_nudges[0]["content"]
                .as_str()
                .unwrap()
                .contains("safety boundary")
        );
        // Turn-wide bounds and project facts survive.
        assert_eq!(turn.rule_regenerations, 1);
        assert!(turn.unpriced_cap_noted);
        assert!(turn.reviewed_paths.contains("src/lib.rs"));
    });
}

#[test]
fn a_continuation_keeps_its_own_round_count_and_the_task_state() {
    for transition in [Transition::AutomaticContinue, Transition::DirectedContinue] {
        with_turn(|turn| {
            turn.reset(transition);
            // The approach changes: failure streaks start over.
            assert_eq!(turn.reliability.consecutive_contract_failures, 0);
            // The task does not: its continuation count bounds automatic resumption.
            assert_eq!(turn.auto_continues, 2, "{transition:?}");
            assert!(turn.solo_todo_noted);
            assert!(turn.automatic_coach_pending);
            assert_eq!(turn.completion_candidate.as_deref(), Some("done"));
            assert_eq!(turn.pending_nudges.len(), 2);
        });
    }
}

#[test]
fn a_rewritten_context_keeps_the_task_state() {
    with_turn(|turn| {
        turn.reset(Transition::ContextRewritten);
        assert_eq!(turn.auto_continues, 2);
        assert_eq!(turn.reliability.consecutive_contract_failures, 2);
        assert!(turn.compaction_failed);
        assert_eq!(turn.completion_candidate.as_deref(), Some("done"));
        assert_eq!(turn.pending_nudges.len(), 2);
        assert!(turn.reviewed_paths.contains("src/lib.rs"));
    });
}

#[test]
fn a_resumed_round_ends_the_rejected_attempt_but_keeps_delivered_advice_and_reviews() {
    with_turn(|turn| {
        let epoch = turn.enrichment_epoch;
        turn.reset(Transition::Resumed);
        assert_eq!(turn.auto_continues, 0);
        assert_eq!(turn.overflow_retries, 0);
        assert!(!turn.automatic_coach_pending);
        assert_eq!(turn.completion_candidate, None);
        assert_eq!(turn.reliability.consecutive_contract_failures, 0);
        assert_eq!(turn.pending_nudges.len(), 1);
        // Same instruction: no new enrichment epoch, and once-per-turn notes stay delivered.
        assert_eq!(turn.enrichment_epoch, epoch);
        assert!(turn.solo_todo_noted);
        assert_eq!(turn.rule_regenerations, 1);
        assert!(turn.reviewed_paths.contains("src/lib.rs"));
    });
}

#[test]
fn only_new_direction_starts_the_recovery_ladder_over() {
    use super::super::progress::{REDIRECT_AT, Rung, Setback};
    for (transition, starts_over) in [
        (Transition::Steering, true),
        (Transition::DirectedContinue, true),
        (Transition::Resumed, true),
        (Transition::AutomaticContinue, false),
        (Transition::ContextRewritten, false),
    ] {
        with_turn(|turn| {
            for _ in 1..REDIRECT_AT {
                assert_eq!(turn.progress.setback(Setback::NoAnswer), Rung::Retry);
            }
            turn.reset(transition);
            let rung = turn.progress.setback(Setback::NoAnswer);
            assert_eq!(rung == Rung::Retry, starts_over, "{transition:?}: {rung:?}");
        });
    }
}

//! What turn state starts over at each transition. One table, so steering, continuation and a
//! rewritten context cannot drift apart as fields are added: `TurnState::reset` destructures every
//! field, and a new field does not compile until it is classified here.
use super::*;

#[cfg(test)]
mod tests;

/// A point at which part of the turn's state stops describing what comes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transition {
    /// A new user instruction replaced or redirected the task.
    Steering,
    /// The harness resumed a stalled turn on its own.
    AutomaticContinue,
    /// The user chose how a stalled turn resumes.
    DirectedContinue,
    /// Compaction or a context rollover rewrote what the model can see.
    ContextRewritten,
    /// Verification or a stop hook rejected the final answer; the same instruction resumes with
    /// that feedback in a new round of the same turn.
    Resumed,
}

impl Transition {
    fn continues(self) -> bool {
        matches!(self, Self::AutomaticContinue | Self::DirectedContinue)
    }
}

impl TurnState<'_> {
    /// Reset the turn-state fields `transition` invalidates. Fields the agent must recompute
    /// (offered tools, portable-rule scope, output budget) are refreshed by
    /// [`Agent::transition_turn`].
    pub(super) fn reset(&mut self, transition: Transition) {
        let steering = transition == Transition::Steering;
        let resumed = transition == Transition::Resumed;
        let continues = transition.continues();
        let rewritten = transition == Transition::ContextRewritten;
        let TurnState {
            // Turn identity and turn-wide bounds: kept across every transition. The portable-rule
            // regeneration bound stays turn-wide so steering cannot reopen it; each resumed round
            // receives its own step budget from `Agent::resume_round`.
            user_goal: _,
            harness: _,
            adapter: _,
            cancel: _,
            turn_started: _,
            budget: _,
            iteration: _,
            rule_root: _,
            rule_catalog: _,
            rule_regenerations: _,
            unpriced_cap_noted: _,
            wrap_up_noted: _,
            turn_effort: _,
            measured: _,
            policy_signature: _,
            enrichment: _,
            // Recomputed by the agent, which owns the inputs.
            names: _,
            rule_state: _,
            max_tokens: _,
            // Facts about the project, not about the context or the instruction.
            reviewed_paths: _,
            prechecked_paths: _,
            revalidated_paths: _,
            // Per-reply recovery owned by `reply_received` and the compaction helpers.
            compacted_without_reply: _,
            // Consumed by the step that set it.
            steering_pending: _,
            final_step: _,
            // Shell results the model can still see: a rewritten context drops them.
            pace,
            // Everything below describes the instruction being worked on.
            pending,
            overflow_retries,
            compaction_failed,
            compaction_retry_floor,
            recovering_output,
            repair,
            stall_continues,
            rejected_edit_previews,
            reasoning_budget,
            todo_recovery,
            solo_todo_noted,
            pending_nudges,
            emissions,
            coach_turn,
            automatic_coach_pending,
            auto_continues,
            planning_notice,
            plan_after_gate,
            reliability,
            stall,
            completion,
            completion_recovery,
            completion_candidate,
            enrichment_epoch,
            progress,
        } = self;
        if steering {
            // A new instruction is a new task: its advice and reviews start over.
            *enrichment_epoch = enrichment_epoch.wrapping_add(1);
            *emissions = Default::default();
            *solo_todo_noted = false;
            *coach_turn = Default::default();
        }
        if steering || resumed || transition == Transition::DirectedContinue {
            // New direction from the user or a verifier: earlier failed recoveries no longer
            // describe the approach. An automatic continue is itself a recovery attempt.
            *progress = Default::default();
        }
        if steering || resumed {
            // The rejected or replaced answer's completion attempt and recovery rounds end. A
            // resumed round keeps the advice already delivered and the coach's repair history.
            completion.reset_for_steering();
            *completion_recovery = Default::default();
            *completion_candidate = None;
            *pending = None;
            crate::agent::run::guidance::reset_for_steering(pending_nudges);
            *rejected_edit_previews = 0;
            *recovering_output = false;
            *repair = Default::default();
            *stall_continues = 0;
            *overflow_retries = 0;
            *reasoning_budget = None;
            *planning_notice = false;
            *plan_after_gate = false;
            *compaction_failed = false;
            *compaction_retry_floor = None;
            *automatic_coach_pending = false;
            *auto_continues = 0;
        }
        if steering || resumed || continues {
            *todo_recovery = Default::default();
            *stall = Default::default();
            reliability.consecutive_contract_failures = 0;
        }
        if rewritten {
            *pace = Default::default();
        }
    }

    /// Compaction or a rollover rewrote the context: the turn and the engine forget what only
    /// the old context held.
    pub(super) fn context_rewritten(&mut self, engine: &mut crate::agent::run::engine::Engine) {
        self.reset(Transition::ContextRewritten);
        engine.evidence_compacted();
    }
}

impl Agent<'_> {
    /// Apply `transition` to the turn and its engine, then refresh what the agent derives from
    /// them: the portable-rule scope and output budget after steering, and the offered tools.
    pub(super) fn transition_turn(
        &mut self,
        turn: &mut TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        transition: Transition,
        backend: &Backend,
        schemas: &mut Vec<Value>,
    ) -> Result<()> {
        use crate::agent::run::engine::RecoveryTransition;
        if transition == Transition::ContextRewritten {
            turn.context_rewritten(engine);
            return Ok(());
        }
        turn.reset(transition);
        match transition {
            Transition::Steering => {
                let instruction = self.session.control_state()["current_instruction"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                turn.rule_state.begin(&turn.rule_root, &instruction);
                let mut control = self.session.control_state();
                turn.rule_state.save(&mut control);
                self.session.set_control_state(control)?;
                engine.transition(RecoveryTransition::Steering);
                turn.max_tokens = backend.output_budget(turn.harness.max_tokens);
            }
            Transition::AutomaticContinue => {
                engine.transition(RecoveryTransition::AutomaticContinue);
            }
            Transition::DirectedContinue | Transition::Resumed => {
                // The approach changes; failure streaks and edit-cycle history are kept.
                engine.transition(RecoveryTransition::DirectedContinue);
            }
            // Returned above: a rewritten context keeps the offered tools.
            Transition::ContextRewritten => return Ok(()),
        }
        turn.names = self.offered();
        *schemas = self.schemas(&turn.names)?;
        Ok(())
    }
}

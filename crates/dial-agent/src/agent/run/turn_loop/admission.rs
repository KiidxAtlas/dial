//! Prompt admission and once-per-turn runtime preparation.

use super::{AdmittedTurn, TurnOrigin, TurnPrompt};
use crate::agent::*;

impl Agent<'_> {
    fn prepare_turn_prompt<'input>(
        &mut self,
        user_text: &'input str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
        origin: TurnOrigin,
    ) -> Result<TurnPrompt<'input>> {
        self.clear_context_rollover();
        ensure!(
            !user_text.trim().is_empty() && user_text.len() <= 262_144,
            "message must be 1..262144 bytes"
        );
        if reset_interrupt {
            self.interrupt.store(false, Ordering::SeqCst);
        }
        let mut prompt_rewrite: Option<String> = None;
        if !matches!(origin, TurnOrigin::Peer | TurnOrigin::Monitor)
            && crate::agent::safety::hooks::wanted(self.tool_hooks(), "user_prompt_submit")
        {
            let configured_hooks = self.tool_hooks();
            let root = self.workspace.root();
            let interrupt = self.interrupt.as_ref();
            let payload = json!({"prompt":user_text,"session_id":self.session.id()});
            let (verdict, notices, context, prompt) =
                crate::agent::safety::hooks::with_heartbeat(ui, || {
                    let outcome = crate::agent::safety::hooks::fire_with(
                        configured_hooks,
                        root,
                        "user_prompt_submit",
                        None,
                        payload,
                        interrupt,
                    );
                    let context = outcome.context_note();
                    (outcome.verdict, outcome.notices, context, outcome.prompt)
                });
            prompt_rewrite = prompt;
            self.queue_hook_context(context);
            notices.iter().for_each(|n| {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Hook,
                    n,
                ))
            });
            match verdict {
                crate::agent::safety::hooks::Verdict::Block { reason } => {
                    bail!("message blocked by a user_prompt_submit hook: {reason}")
                }
                crate::agent::safety::hooks::Verdict::Continue { output } => {
                    if !output.is_empty() {
                        ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                            crate::agent::ui::ui_event::EventKind::Hook,
                            format!("hook: {output}"),
                        ));
                    }
                }
            }
        }
        if let Some(text) = &prompt_rewrite {
            ensure!(
                !text.trim().is_empty() && text.len() <= 262_144,
                "rewritten message must be 1..262144 bytes"
            );
        }
        let submitted_text = user_text;
        let user_text = prompt_rewrite.as_deref().unwrap_or(user_text);
        // Turn optimizer: steering left from an earlier turn, then this message, pass through the
        // drafter before anything else reads them. The typed text stays the turn's instruction;
        // an accepted draft is attached to the message as an interpretation.
        let typed_text = user_text;
        let drafted = if self.turn_optimizer_enabled() {
            self.draft_leftover_steering(ui)?;
            match origin {
                TurnOrigin::User => self.draft_turn_text(typed_text, ui)?,
                TurnOrigin::Harness | TurnOrigin::Peer | TurnOrigin::Monitor => None,
            }
        } else {
            None
        };
        Ok(TurnPrompt {
            typed: match prompt_rewrite {
                Some(text) => std::borrow::Cow::Owned(text),
                None => std::borrow::Cow::Borrowed(submitted_text),
            },
            drafted,
        })
    }

    fn prepare_turn_runtime(&mut self, user_text: &str, ui: &mut dyn Ui) -> Result<()> {
        if std::mem::take(&mut self.legacy_memory_migrated) {
            ui.diagnostic("Migrated legacy split memory tool selection to the single memory tool for this runtime; admitted history is unchanged.");
        }
        self.task_policy = crate::learn::task_policy::Policy::for_prompt(
            user_text,
            self.config.reasoning_guard.max_pre_action_secs,
        );
        self.request_phase = crate::adaptive::State::begin(self.task_policy.kind, user_text);
        self.workflow_started = Instant::now();
        self.efficiency_metrics = Default::default();
        *self.workflow_metrics.borrow_mut() = Default::default();
        self.refresh_version(ui, user_text)?;
        // User-managed skill imports/toggles take effect between turns, including RPC and CLI changes.
        if let Some(layout) = &self.layout {
            let catalog = if self.modes.skillful == Some(false) {
                String::new()
            } else {
                let visible: Vec<_> = skills::discover(layout)
                    .into_iter()
                    .filter(|skill| self.modes.launch.skill_visible(&skill.name))
                    .collect();
                skills::catalog_block(&visible)
            };
            if catalog != self.parts.skills {
                self.parts.skills = catalog;
                self.rebuild_system();
            }
        }
        // A step this local model reached in an earlier session applies from the first request.
        // The saved record for this turn is read once, before the prompt and the seed use it.
        self.refresh_turn_record();
        self.seed_local_escalation();
        self.begin_local_turn(ui);
        // The probed context can change the local profile and its routing budget.
        self.probe_context();
        self.begin_tool_routing(user_text);
        // Offered tools can change with version/harness refresh or skill toggles; rebuild only if content changed.
        self.rebuild_system();
        for notice in self.ext.take_notices() {
            ui.notice(&notice);
        }
        if let Some(served) = self.context_cap {
            self.local_context_advice(served, ui);
        }
        self.step_probe = self
            .store
            .map(crate::learn::probe::load)
            .transpose()?
            .flatten()
            .filter(crate::learn::probe::informative);
        self.session.repair_incomplete_tools()?;
        Ok(())
    }

    pub(super) fn admit_turn<'input>(
        &mut self,
        user_text: &'input str,
        ui: &mut dyn Ui,
        reset_interrupt: bool,
        prepared: Option<Value>,
        origin: TurnOrigin,
    ) -> Result<AdmittedTurn<'input>> {
        let prompt = self.prepare_turn_prompt(user_text, ui, reset_interrupt, origin)?;
        let typed_text = prompt.typed();
        let user_text = typed_text;
        let drafted = &prompt.drafted;
        self.prepare_turn_runtime(user_text, ui)?;
        let mut message = if let Some(message) = prepared {
            message
        } else if let Some(layout) = &self.layout {
            // References expand from what the user typed, never from model-written text.
            let (message, warnings) = crate::agent::window::context::expand_message(
                &layout.project_root,
                typed_text,
                &self.interrupt,
            );
            for warning in warnings {
                ui.notice(&warning);
            }
            message
        } else {
            json!({"role":"user","content":typed_text})
        };
        if let Some(drafted) = drafted {
            crate::agent::run::turn_draft::apply_draft(&mut message, typed_text, drafted);
        }
        let checkpoint_turn = self
            .checkpoints
            .as_ref()
            .map(|checkpoints| {
                checkpoints.begin_conversation_turn(
                    &crate::agent::window::prompt::truncate_chars(typed_text, 60),
                    self.session.id(),
                )
            })
            .transpose()?;
        message["dial_checkpoint"] = json!({"session": self.session.id(), "turn": checkpoint_turn});
        self.step_checks = StepChecks {
            autonomous_continuation: !matches!(origin, TurnOrigin::User),
            turn: checkpoint_turn,
            visual: if self.hermetic {
                crate::agent::verify::visual::Review::default()
            } else {
                crate::agent::verify::visual::Review::new(user_text)
            },
            // Autonomous and peer continuations are not user requests to run anything.
            targets: if self.hermetic
                || !matches!(origin, TurnOrigin::User)
                || !self.config.verification.enabled
                || !self.config.verification.target_evidence
            {
                crate::agent::verify::targets::Targets::default()
            } else {
                crate::agent::verify::targets::Targets::resolve(&self.workspace, user_text)
            },
            ..StepChecks::default()
        };
        if self
            .config
            .verification
            .requirements
            .values()
            .any(|requirement| requirement.visual)
        {
            self.step_checks.visual.require_rendered();
        }
        if !self.hermetic
            && message.get("dial_images").is_some()
            && self
                .backend
                .as_ref()
                .is_some_and(Backend::accepts_image_input)
        {
            self.step_checks.visual.user_image([0, 0, 0]);
        }
        for reference in message["dial_images"].as_array().into_iter().flatten() {
            ui.image(reference);
        }
        self.repair_tracker.clear();
        self.repair_sequence = 0;
        self.ext.fire_lifecycle(&LifecycleEvent {
            event: "turn_started".into(),
            session_id: Some(self.session.id().to_owned()),
            turn_id: checkpoint_turn.map(|turn| turn.to_string()),
            tool_name: None,
            is_error: None,
        });
        self.trajectory = match self.store {
            Some(store) if self.version.id != 0 => {
                let task = self.domain_override.as_ref().map_or_else(
                    || format!("session:{}", self.session.id()),
                    |_| "selfplay".to_owned(),
                );
                let domain = self
                    .domain_override
                    .clone()
                    .unwrap_or_else(|| self.config.use_case.clone());
                let id = store.begin_trajectory(self.version.id, &domain, &task, user_text)?;
                Some((id, domain, task))
            }
            _ => None,
        };
        self.assistance.begin(user_text);
        self.reasoning_recovery = crate::agent::workflow::reasoning::Recovery::default();
        self.logging = self.store.is_some();
        self.admit_turn_message(message, &prompt, origin, ui)?;
        if !self.hermetic
            && self.modes.launch.unattended
            && matches!(origin, TurnOrigin::User)
            && self.config.efficiency.preserve_task_inputs
            && self
                .session
                .control_state()
                .get("preserved_inputs")
                .is_none()
            && let (Some(checkpoints), Some(turn)) = (&self.checkpoints, checkpoint_turn)
        {
            // Silent: an up-front "copies exist" note made changing the originals look safe.
            // `note_preserved_input_changes` names the copy only once an original is lost.
            let preserved = crate::tools::preserve_task_inputs(&self.workspace, checkpoints, turn);
            let mut state = self.session.control_state();
            state["preserved_inputs"] = preserved;
            self.session.set_control_state(state)?;
        }
        let names = self.offered();
        let schemas = self.schemas(&names)?;
        crate::tools::surface::contracts::validate_history(
            &self.session.messages(),
            &schemas,
            &self.ext,
        )?;
        self.last_trajectory = self.trajectory.as_ref().map(|(id, _, _)| *id);
        let max_steps = if !self.hermetic && self.config.harness.max_steps == 0 {
            0
        } else {
            self.version.harness.max_steps
        };
        self.record_turn_baseline(ui)?;
        self.prewalk_resume()?;
        self.plan_model_resume()?;
        self.switch_resume();
        Ok(AdmittedTurn {
            prompt,
            names,
            schemas,
            max_steps,
            checkpoint_turn,
        })
    }

    /// Capture the configured pre-edit baseline without spending a repair allowance.
    fn record_turn_baseline(&mut self, ui: &mut dyn Ui) -> Result<()> {
        if !self.hermetic
            && self.config.verification.enabled
            && self.config.verification.automatic_checks
            && matches!(
                self.task_policy.kind,
                crate::learn::task_policy::Kind::Editing
                    | crate::learn::task_policy::Kind::Debugging
            )
        {
            let baseline = self
                .config
                .verification
                .commands
                .iter()
                .take(8)
                .map(|check| crate::agent::verify::verification::FocusedCheck {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    timeout_secs: check.timeout_secs,
                })
                .collect::<Vec<_>>();
            if !baseline.is_empty() {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Verification,
                    "Recording the configured check baseline before edits.",
                ));
                let failures = self.run_checks(&baseline, ui).failures;
                self.step_checks.evidence.capture_baseline();
                let baseline_context = if failures.is_empty() {
                    "Configured check baseline passed before editing. Preserve these behaviors and rerun relevant checks after the change.".to_owned()
                } else {
                    format!(
                        "Configured baseline before editing (these failures predate this turn's edits):\n{}\nDeclared behavioral checks identify requested scope. Preserve and disclose unchanged unrelated failures; do not broaden the task to repair them.",
                        failures.join("\n")
                    )
                };
                self.record(
                    0,
                    "behavioral_baseline",
                    json!({"checks":baseline.len(),"failures":failures.len()}),
                )?;
                self.push(json!({"role":"user","content":baseline_context}))?;
            }
        }
        Ok(())
    }
}

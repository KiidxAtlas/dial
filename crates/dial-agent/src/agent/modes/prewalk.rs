//! `/prewalk` and `--prewalk`: a todo-gated, one-shot model handoff.
//!
//! The active model plans. Once a todo list exists and the first successful `edit` or `write`
//! lands, the session continues on the target model (`@small` by default). The handoff persists
//! in the session's control state, so later turns and rebuilt agents stay on the target until
//! `/prewalk restart` or a model change. Nudges are request-local `dial_transient` messages.

use crate::agent::{Agent, Ui, make_backend};
use crate::llm::model_config::ModelSelection;
use crate::llm::provider::Backend;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PLAN_NUDGE: &str = "STOP: In NEXT reply, before further exploration, write complete plan. Enough known; do not defer.\n\nPlan first; explicit, comprehensive; reference for remainder:\n- Remaining execution-order steps: exact files, symbols, commands, checks.\n- Risks, edge cases; verify each landed: specific commands, expected outputs. NEVER modify tests or verification assets to pass checks.\n- Already done, brief; prevent repetition.\n\nThorough, concrete. Tools may verify details only after plan.\n\nThen, same reply and only after complete plan, use todo tool to capture 5–9 items: one per MEANINGFUL step; each concrete target + verification. Only code-changing or code-verifying steps; exclude reporting, bookkeeping, cleanup-ceremony, release-note items. Todo serves task, not reverse: reality/item conflict → fix actual problem, not checklist.\n\nCheckpoint, not final answer: after todo list, continue task; do not stop on plan alone.";

const CHECKLIST: &str = "Before task complete, verify:\n\n- Consistency: If a pattern, signature, or check changed in one place, grep every other call site or duplicate copy needing identical change. A fix at only some matching sites fails.\n- Scope: If diff exceeds the minimal issue-resolving change, confirm behavior unchanged outside the reported issue. Prefer the smallest correct diff over a broader rewrite.\n- Verification: Run the issue's full test module or file, not only the expected-to-flip test. A sibling-test-breaking change fails.\n\nDo not claim task complete until all three checks done.";

const CONTINUE: &str = "Continue task now; do not end turn here.";

/// Session-scoped prewalk state. Persisted under the `"modes"` control-state key.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Coordinator {
    /// Target armed for the next edit/write. `None` when no handoff is pending.
    armed: Option<ModelSelection>,
    /// The plan nudge was injected for the armed handoff.
    plan_injected: bool,
    /// A continuation is owed at the next turn end that has no tool results.
    continue_pending: bool,
    /// A successful `todo` call has happened since arming.
    todo_seen: bool,
    /// The model the session continues on after the switch. The backend is rebuilt from it each turn.
    pub(crate) handoff: Option<ModelSelection>,
    /// The startup arm (`--prewalk`) has run for this session.
    started: bool,
    /// Steering queued for the next request (the plan nudge after arming). Persisted until delivered.
    queued: Vec<Value>,
    /// `(tool name, ok)` for dispatches made while armed in the current turn.
    #[serde(skip)]
    results: Vec<(String, bool)>,
}

/// Outcome of `/prewalk restart`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartOutcome {
    /// A different target is already armed; the request was turned into an arm.
    Rejected,
    /// The handoff is armed again after restoring the planning model.
    Armed,
    /// The target resolves to the planning model, so no handoff was armed.
    Reset,
}

/// Result of `/prewalk restart`, with the labels the command reports.
#[derive(Debug, Clone)]
pub struct RestartReport {
    pub outcome: RestartOutcome,
    pub source: String,
    pub target: String,
    pub notices: Vec<String>,
}

fn nudge(content: &str) -> Value {
    json!({"role": "user", "content": content, "dial_transient": true})
}

fn is_plan_nudge(message: &Value) -> bool {
    message["content"].as_str() == Some(PLAN_NUDGE)
}

fn label(selection: &ModelSelection) -> String {
    format!("{}/{}", selection.provider, selection.model)
}

fn same_model(a: &ModelSelection, b: &ModelSelection) -> bool {
    a.provider == b.provider && a.model == b.model
}

fn noop_notice(target: &ModelSelection) -> String {
    format!(
        "Prewalk: target {} already matches the active model; nothing to switch.",
        label(target)
    )
}

impl Agent<'_> {
    /// The model the session is on: the handoff target once switched, else the effective config.
    fn prewalk_active(&self) -> ModelSelection {
        self.modes
            .prewalk
            .handoff
            .clone()
            .unwrap_or_else(|| self.effective_config().selection())
    }

    fn prewalk_is_noop(&self, target: &ModelSelection) -> bool {
        same_model(&self.prewalk_active(), target)
    }

    fn prewalk_backend(&self, target: &ModelSelection) -> Result<Backend> {
        let config = self.effective_config().with_selection(target);
        let mut backend = make_backend(&config)?.with_session_id(self.session.id().to_owned());
        backend.set_usage_scope(Some(self.usage_scope.clone()));
        Ok(backend)
    }

    fn prewalk_clear(&mut self) {
        let p = &mut self.modes.prewalk;
        p.armed = None;
        p.plan_injected = false;
        p.continue_pending = false;
        p.todo_seen = false;
        p.queued.clear();
    }

    /// Whether a handoff is armed and waiting for its first edit/write.
    pub fn prewalk_is_armed(&self) -> bool {
        self.modes.prewalk.armed.is_some()
    }

    /// Resolves `@main`, `@<role>`, or a bare model name against the effective config.
    pub fn prewalk_resolve(&self, spec: &str) -> Result<ModelSelection> {
        match spec.strip_prefix('@') {
            Some("main") => Ok(self.effective_config().selection()),
            Some(role) => self
                .role_selection(role)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "Model \"{spec}\" not found: no model is assigned to the {role} role. Assign one in /models → Roles, or name a model directly."
                    )
                }),
            None => {
                let mut selection = self.effective_config().selection();
                selection.model = spec.to_owned();
                Ok(selection)
            }
        }
    }

    /// Arms a handoff to `target`. Returns whether the armed target matches the
    /// request, plus the notices to show.
    pub fn prewalk_arm(&mut self, target: ModelSelection) -> Result<(bool, Vec<String>)> {
        if let Some(active) = self.modes.prewalk.armed.clone() {
            let note = format!(
                "Prewalk: already armed for {}, waiting for the first edit/write.",
                label(&active)
            );
            return Ok((same_model(&active, &target), vec![note]));
        }
        if self.prewalk_is_noop(&target) {
            return Ok((false, vec![noop_notice(&target)]));
        }
        // Build the target now so a missing key or endpoint fails at arm time, not mid-turn.
        let mut probe = self.prewalk_backend(&target)?;
        probe.release();
        let p = &mut self.modes.prewalk;
        p.armed = Some(target.clone());
        p.plan_injected = true;
        p.continue_pending = true;
        p.todo_seen = false;
        p.queued.push(nudge(PLAN_NUDGE));
        self.save_modes()?;
        Ok((
            true,
            vec![format!(
                "Prewalk: armed for {} — will switch at the first edit/write once the todo list exists.",
                label(&target)
            )],
        ))
    }

    /// Drops a pending handoff without switching; the active model stays.
    pub fn prewalk_disarm(&mut self) -> Result<Vec<String>> {
        let Some(active) = self.modes.prewalk.armed.clone() else {
            return Ok(Vec::new());
        };
        self.prewalk_clear();
        self.save_modes()?;
        Ok(vec![format!(
            "Prewalk: disarmed; staying on the active model instead of switching to {}.",
            label(&active)
        )])
    }

    /// Restores the planning model and re-arms the handoff to `target_spec`.
    pub fn prewalk_restart(&mut self, target_spec: &str) -> Result<RestartReport> {
        let target = self.prewalk_resolve(target_spec)?;
        let source = self.prewalk_resolve("@main")?;
        let report = |outcome, notices| RestartReport {
            outcome,
            source: label(&source),
            target: label(&target),
            notices,
        };
        if let Some(active) = self.modes.prewalk.armed.clone()
            && !same_model(&active, &target)
        {
            let (_, notices) = self.prewalk_arm(target.clone())?;
            return Ok(report(RestartOutcome::Rejected, notices));
        }
        if self.modes.prewalk.handoff.is_some() {
            self.modes.prewalk.handoff = None;
            self.prewalk_restore_main()?;
        }
        let (outcome, notices) = if self.prewalk_is_armed() {
            if self.prewalk_is_noop(&target) {
                self.prewalk_clear();
                (RestartOutcome::Reset, vec![noop_notice(&target)])
            } else {
                (RestartOutcome::Armed, Vec::new())
            }
        } else {
            let (armed, notices) = self.prewalk_arm(target.clone())?;
            let outcome = if armed {
                RestartOutcome::Armed
            } else {
                RestartOutcome::Reset
            };
            (outcome, notices)
        };
        self.save_modes()?;
        Ok(report(outcome, notices))
    }

    /// Rebuilds the planning model's backend after a handoff is cleared.
    fn prewalk_restore_main(&mut self) -> Result<()> {
        let mut backend =
            make_backend(self.effective_config())?.with_session_id(self.session.id().to_owned());
        backend.set_usage_scope(Some(self.usage_scope.clone()));
        if let Some(mut old) = self.backend.replace(backend) {
            old.release();
        }
        Ok(())
    }

    /// `--prewalk` / `--prewalk-into`: arms the handoff once per session.
    pub fn prewalk_startup(&mut self, spec: &str) -> Result<Vec<String>> {
        if self.modes.prewalk.started {
            return Ok(Vec::new());
        }
        self.modes.prewalk.started = true;
        let target = self.prewalk_resolve(spec)?;
        let (_, notices) = self.prewalk_arm(target)?;
        self.save_modes()?;
        Ok(notices)
    }

    /// Turn start: clears stale observations and rebuilds the handoff backend so the switched model
    /// persists across turns and rebuilt agents.
    pub(crate) fn prewalk_resume(&mut self) -> Result<()> {
        self.modes.prewalk.results.clear();
        let Some(target) = self.modes.prewalk.handoff.clone() else {
            return Ok(());
        };
        let next = self.prewalk_backend(&target)?;
        if let Some(mut old) = self.backend.replace(next) {
            old.release();
        }
        Ok(())
    }

    /// Hands the session to `target` now and keeps it there across turns (plan-yolo execution
    /// handoff). Uses the same handoff slot that `prewalk_resume` rebuilds from.
    pub(crate) fn prewalk_switch(&mut self, target: ModelSelection) -> Result<()> {
        let next = self.prewalk_backend(&target)?;
        self.modes.prewalk.handoff = Some(target);
        if let Some(mut old) = self.backend.replace(next) {
            old.release();
        }
        Ok(())
    }

    /// Records one dispatched tool result while a handoff is armed.
    pub(crate) fn prewalk_observe(&mut self, name: &str, ok: bool) {
        if self.prewalk_is_armed() {
            self.modes.prewalk.results.push((name.to_owned(), ok));
        }
    }

    /// Start of each request: moves queued steering into this request's nudges, then runs turn-end
    /// logic for the previous step's tool results.
    pub(crate) fn prewalk_step_start(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        nudges: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> Result<()> {
        if !self.modes.prewalk.queued.is_empty() {
            nudges.append(&mut self.modes.prewalk.queued);
            self.save_modes()?;
        }
        if !self.modes.prewalk.results.is_empty() {
            self.prewalk_turn_end(backend, names, nudges, ui)?;
        }
        Ok(())
    }

    /// Turn-end logic. Returns whether a continuation nudge was queued,
    /// so a text-only turn keeps going instead of finishing.
    pub(crate) fn prewalk_turn_end(
        &mut self,
        backend: &mut Backend,
        names: &[String],
        nudges: &mut Vec<Value>,
        ui: &mut dyn Ui,
    ) -> Result<bool> {
        let results = std::mem::take(&mut self.modes.prewalk.results);
        let Some(target) = self.modes.prewalk.armed.clone() else {
            return Ok(false);
        };
        if self.prewalk_is_noop(&target) {
            nudges.retain(|m| !is_plan_nudge(m));
            self.prewalk_clear();
            self.save_modes()?;
            ui.notice(&noop_notice(&target));
            return Ok(false);
        }
        let mut nudged = false;
        {
            let p = &mut self.modes.prewalk;
            if results.iter().any(|(name, ok)| name == "todo" && *ok) {
                p.todo_seen = true;
            }
            if p.plan_injected && !results.is_empty() {
                p.continue_pending = true;
            } else if p.continue_pending {
                p.continue_pending = false;
                nudges.push(nudge(CONTINUE));
                nudged = true;
            }
        }
        let gate_open = self.modes.prewalk.todo_seen || !names.iter().any(|name| name == "todo");
        let action = if gate_open {
            results
                .iter()
                .find(|(name, ok)| *ok && (name == "edit" || name == "write"))
                .cloned()
        } else {
            None
        };
        let Some((tool, _)) = action else {
            self.save_modes()?;
            return Ok(nudged);
        };

        nudges.retain(|m| !is_plan_nudge(m));
        let next = self.prewalk_backend(&target)?;
        backend.release();
        *backend = next;
        self.prewalk_clear();
        self.modes.prewalk.handoff = Some(target.clone());
        self.save_modes()?;
        ui.event(&crate::agent::ui::ui_event::UiEvent::new(
            crate::agent::ui::ui_event::EventKind::Model,
            format!(
                "Prewalk: switched to {} after first {tool} call.",
                label(&target)
            ),
        ));
        nudges.push(nudge(CHECKLIST));
        Ok(true)
    }
}

//! Admit ambient context once and commit recall only after its message is durable.

use super::{TurnOrigin, TurnPrompt};
use crate::agent::*;

impl Agent<'_> {
    pub(super) fn admit_turn_message(
        &mut self,
        mut message: Value,
        prompt: &TurnPrompt<'_>,
        origin: TurnOrigin,
        ui: &mut dyn Ui,
    ) -> Result<()> {
        let typed_text = prompt.typed();
        let user_text = typed_text;
        if let (Some(hint), Some(content)) = (
            self.skill_hint(user_text),
            message["content"].as_str().map(str::to_owned),
        ) {
            message["content"] = json!(format!("{content}\n\n{hint}"));
        }
        // Recalled notes and verified repair patterns are decided once, for this message, and stored with it.
        // Recomputing them on every request (or putting them in the system prompt, as hints once were) would
        // change the text in front of the whole conversation and discard the provider's prompt cache.
        let mut context = self.experience_context(user_text, ui);
        if matches!(origin, TurnOrigin::User) {
            // Goal commands submit their original text, not a rendered harness prompt.
            // Keep mode guidance in ambient context so drafting and provenance see only
            // the admitted user input, including literal XML characters.
            let goal_context = if self.modes.goal.interview {
                Some(crate::agent::prompts::modes::goal::GUIDED_RULES.to_owned())
            } else {
                self.modes
                    .goal
                    .goal
                    .as_ref()
                    .filter(|goal| goal.status == crate::agent::modes::goal::Status::Active)
                    .map(crate::agent::modes::goal::active_prompt)
            };
            if let Some(goal_context) = goal_context {
                crate::agent::workflow::efficiency::append_ambient(
                    &mut context,
                    &goal_context,
                    &self.session.messages(),
                );
            }
        }
        self.invalidate_exploration(
            "new external user turn superseded the retained checkpoint",
            ui,
        )?;
        let recall_authority = memory_authority(&self.session, 0);
        let prepared_recall = match self.prepare_selected_memory(user_text, ui) {
            Ok(prepared) => prepared,
            Err(error) => {
                ui.diagnostic(&format!("Selected memory recall unavailable: {error:#}"));
                None
            }
        };
        // Recall attaches only facts not already shown on this branch; earlier ones stay in the
        // history of the message they were attached to, so nothing is re-appended.
        if let Some(saved) = prepared_recall
            .as_ref()
            .and_then(|prepared| prepared.context.as_deref())
        {
            context.push_str("\n\n");
            context.push_str(saved);
        }
        if let Some(memory) = &self.memory_runtime {
            let authority = memory_authority(&self.session, 0);
            memory.bind(authority.clone());
            match memory.instructions(&authority) {
                Ok(Some(saved)) if !saved.trim().is_empty() => context.push_str(&format!(
                    "\n\n<selected-memory trust=\"untrusted\">\n{saved}\n</selected-memory>"
                )),
                Ok(_) => {}
                Err(error) => ui.diagnostic(&format!(
                    "Selected memory context is unavailable: {error:#}"
                )),
            }
        }
        if !self.hermetic && crate::agent::verify::visual::useful(user_text) {
            context.push_str("\n\n");
            context.push_str(crate::agent::verify::visual::INITIAL_GUIDANCE);
        }
        if !self.config.verification.requirements.is_empty() {
            context.push_str("\n\nDeclared verification requirements (evidence mapping, not permission or proof of full task coverage):\n");
            let commands: Vec<_> = self
                .config
                .verification
                .commands
                .iter()
                .filter(|command| {
                    self.config
                        .verification
                        .requirements
                        .values()
                        .any(|requirement| requirement.checks.contains(&command.name))
                })
                .collect();
            context.push_str(&serde_json::to_string(&json!({
                "requirements": self.config.verification.requirements,
                "configured_commands": commands,
            }))?);
            context.push_str("\nRun the named configured checks against the final result; required visuals need delivered browser pixels. Missing, stale, failed or unavailable evidence remains uncovered. Do not weaken checks or edit code to hide environment failures.");
        }
        self.append_provenance_context(&mut context, user_text, ui);
        if !context.is_empty() {
            message["dial_recall"] = json!(context);
        }
        message["dial_context_request"] = json!(true);
        self.session.append(message)?;
        self.training_capture
            .set_source_turn(self.session.branch_leaf());
        let recovery = self.session.take_recovery_diagnostics();
        if !recovery.is_empty() {
            let count = recovery.len();
            // Journal identifiers/details are untrusted source data. Give the model only a
            // bounded controller-generated classification, never damaged bytes as instructions.
            let sample: Vec<_> = recovery
                .iter()
                .take(16)
                .map(|issue| json!({"kind":issue.kind,"line":issue.line}))
                .collect();
            let sample = Value::Array(sample);
            let notice = format!(
                "Session recovery: {count} journal issue(s) were detected. Source bytes were preserved; resumed history may be incomplete. Inspect history before relying on missing context."
            );
            ui.notice(&notice);
            self.record(
                0,
                "session_recovery",
                json!({"count":count,"sample":sample}),
            )?;
            self.push(json!({"role":"user","content":format!("{notice}\nRecovery classifications (at most 16): {sample}"),"dial_session_recovery":true}))?;
        }
        // A plan approved between turns (e.g. /plan-review) must reach this turn's first request.
        if let Some(plan) = self.pending_plan.take() {
            self.push(json!({"role":"user","content":format!("Approved plan:\n{plan}\nProceed with implementation.")}))?;
        }
        if let Some(prepared) = prepared_recall {
            prepared.commit(&recall_authority);
        }
        if let Some(memory) = &self.memory_runtime {
            memory.bind(memory_authority(&self.session, 0));
        }
        let mut state = self.session.control_state();
        // Autonomous/peer/monitor continuations are not new user instructions.
        if matches!(origin, TurnOrigin::User) {
            crate::agent::run::intent::record(
                &mut state,
                typed_text,
                json!(self.session.branch_leaf()),
                crate::agent::run::intent::Kind::Turn,
            );
            state["current_instruction_entry_id"] = json!(self.session.branch_leaf());
            if state["original_goal"].is_null() {
                state["original_goal"] = json!(crate::agent::window::prompt::truncate_chars(
                    user_text, 4096
                ));
            }
            state["current_instruction"] = json!(user_text);
        }
        state["turn_outcome"] = Value::Null;
        state["steering_active"] = json!(false);
        self.session.set_control_state(state)?;
        if let Some(advice) = self.laya_advice(user_text, ui) {
            self.push(json!({"role":"user","content":format!("Laya retrieval advice (untrusted, advisory): {advice}")}))?;
        }
        Ok(())
    }

    fn append_provenance_context(
        &mut self,
        context: &mut String,
        user_text: &str,
        ui: &mut dyn Ui,
    ) {
        // Admission tracks the model-visible branch even when optional economy is disabled.
        let ambient_history = self.session.messages();
        if let Some(memory) = &self.memory {
            // The lookup is a search query, not the prompt: a pasted file or log must not fail the turn.
            let query = crate::agent::window::prompt::truncate_chars(user_text, RECALL_QUERY_CHARS);
            match memory.context_lookup(
                &query,
                self.config.memory.context_limit,
                crate::agent::workflow::efficiency::byte_limit(
                    self.config.memory.context_max_bytes,
                )
                .min(if self.config.efficiency.enabled {
                    crate::agent::workflow::efficiency::byte_limit(
                        self.config.efficiency.ambient_context_bytes,
                    )
                } else {
                    usize::MAX
                }),
            ) {
                Ok(recalled) => crate::agent::workflow::efficiency::append_ambient(
                    context,
                    &recalled,
                    &ambient_history,
                ),
                Err(error) => {
                    ui.diagnostic(&format!("Provenance memory lookup unavailable: {error:#}"))
                }
            }
        }
        if self.config.codebase.ambient_context
            && let Some(saved) = self.codebase_context(
                &crate::agent::window::prompt::truncate_chars(user_text, RECALL_QUERY_CHARS),
                ui,
            )
        {
            crate::agent::workflow::efficiency::append_ambient(context, &saved, &ambient_history);
        }
        if let Some(hints) = self.repair_context(user_text, ui) {
            if !context.is_empty() {
                context.push_str("\n\n");
            }
            context.push_str(&hints);
        }
        if !self.hermetic
            && self.config.assistance.repair_hints
            && let Some(store) = self.store
        {
            let repairs = match store
                .recent_executable_repairs(&self.workspace.root().to_string_lossy(), 16)
            {
                Ok(repairs) => repairs,
                Err(error) => {
                    ui.diagnostic(&format!("executable repair lookup unavailable: {error:#}"));
                    Vec::new()
                }
            };
            let terms: Vec<String> = user_text
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|term| term.len() >= 4)
                .take(32)
                .map(str::to_lowercase)
                .collect();
            let related: Vec<_> = repairs
                .into_iter()
                .filter(|(_, record)| {
                    let searchable = format!(
                        "{} {} {}",
                        record["before"]["objective"],
                        record["before"]["command"],
                        record["changes"]
                    )
                    .to_lowercase();
                    terms.iter().any(|term| searchable.contains(term))
                })
                .take(3)
                .collect();
            let mut hash_budget = 2 * 1024 * 1024;
            let matching: Vec<i64> = related
                .iter()
                .filter_map(|(trajectory, payload)| {
                    let record: crate::storage::repair_evidence::RepairRecord =
                        serde_json::from_value(payload.clone()).ok()?;
                    crate::agent::verify::repair_evidence::matches_current(
                        &record,
                        &self.workspace,
                        &mut hash_budget,
                    )
                    .then_some(*trajectory)
                })
                .collect();
            if let Some(repairs) =
                crate::agent::verify::repair_evidence::retrieval_text(&related, 4096)
            {
                if !context.is_empty() {
                    context.push_str("\n\n");
                }
                context.push_str(&repairs);
                context.push_str(&format!("\nExact failed-state file hashes currently match source trajectories {matching:?}. Other records are analogous or unchecked, not directly applicable. Re-run the executable check; file hashes do not verify the environment."));
            }
        }
    }
}

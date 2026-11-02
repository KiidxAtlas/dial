//! Journal and admit the exact assistant batch before any call runs.
use super::*;

impl Agent<'_> {
    pub(super) fn prepare_batch(
        &mut self,
        turn: &TurnState<'_>,
        engine: &mut crate::agent::run::engine::Engine,
        schemas: &[Value],
        ui: &mut dyn Ui,
        round: &Round,
    ) -> Result<BatchState> {
        let reply = &round.reply;
        let single_call = round.single_call;
        let repeat_streak = &round.repeat_streak;
        let narration_warning = engine.narration.observe(reply);
        self.edit_preview_intercepted = false;
        let mut assistant =
            crate::tools::surface::contracts::assistant_message(reply, schemas, &self.ext);
        crate::agent::window::file_handles::capture_calls(&self.workspace, &mut assistant);
        self.push(assistant)?;
        if !reply.text_calls_lifted
            && (!single_call || reply.tool_calls.len() <= 1)
            && engine.pending_refusal().is_none()
        {
            for call in reply.tool_calls.iter().skip(1) {
                if call_form_error(call, schemas, &turn.names).is_none()
                    && engine.rejection_at(&self.workspace, call).is_none()
                {
                    self.warm_speculative_read(call);
                }
            }
        }
        // Past the stronger guidance, an identical repeat is answered with its earlier result
        // instead of running again. Only the repeated call set qualifies; anything else runs.
        let duplicates = repeat_streak
            .as_ref()
            .filter(|repeat| {
                repeat.count > crate::agent::safety::stream_guards::TOOL_CALL_LOOP_THRESHOLD
            })
            .and_then(|repeat| repeat.short_circuit(&reply.tool_calls));
        self.drain_background_check(ui, false);
        if duplicates.is_some() {
            ui.event(&crate::agent::ui::ui_event::UiEvent::new(crate::agent::ui::ui_event::EventKind::Recovery, "Repeated tool call not executed; the earlier identical result is already in context"));
        }
        if self.config.experimental.batch_approvals
            && !reply.text_calls_lifted
            && !self.hermetic
            && !self.plan_mode
            && !single_call
            && duplicates.is_none()
            && engine.pending_refusal().is_none()
            && self.approval == crate::config::types::Approval::Ask
        {
            let prepared: Vec<_> = reply
                .tool_calls
                .iter()
                .take(32)
                .filter_map(|call| {
                    if !matches!(call.name.as_str(), "edit" | "write" | "ast_edit")
                        || call_form_error(call, schemas, &turn.names).is_some()
                        || engine.rejection_at(&self.workspace, call).is_some()
                        || crate::agent::safety::hooks::wanted_for_tool(
                            self.tool_hooks(),
                            "pre_tool_use",
                            &call.name,
                        )
                    {
                        return None;
                    }
                    let mut effective = call.clone();
                    crate::agent::window::file_handles::expand(&self.workspace, &mut effective)
                        .ok()?;
                    if self
                        .always
                        .iter()
                        .any(|rule| rule.matches(&effective, self.workspace.root()))
                    {
                        return None;
                    }
                    Some(effective)
                })
                .collect();
            if prepared.len() > 1 {
                let choices = ui.approve_batch(&prepared, self.interrupt.as_ref());
                self.pending_batch_approvals = prepared
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| (call, choices.get(index) == Some(&true)))
                    .collect();
            }
        }
        Ok(BatchState {
            narration_warning,
            duplicates,
            ..Default::default()
        })
    }
}

//! Publish settled evidence, media and file handles in provider conversation order.
use super::*;

/// Truncated-output artifacts remembered for the recovery metric; older ones are forgotten.
const TRUNCATED_ARTIFACTS_KEPT: usize = 256;

pub(super) use crate::agent::run::dispatch::result_budget::ResultBudget;

impl Agent<'_> {
    pub(super) fn publish_call_result(
        &mut self,
        turn: &mut TurnState<'_>,
        context: &mut DispatchContext<'_>,
        step: u32,
        settled: SettledCall<'_>,
        budget: &mut ResultBudget,
        pending: &[usize],
    ) -> Result<()> {
        let backend = &mut *context.backend;
        let ui = &mut *context.ui;
        let SettledCall {
            call,
            mut output,
            image,
            tool_seconds,
            status,
            duplicate,
            is_duplicate,
            search_note,
            repro_advice,
        } = settled;
        self.record(
            step,
            if output.is_error {
                "tool_error"
            } else {
                "tool_result"
            },
            json!({"name":call.name,"output":truncate(&output.text, 4096)}),
        )?;
        let (artifact, retain_notice) = match self.workspace.artifacts() {
            Some(store) => match store.retain(&output.text) {
                Ok(uri) => (Some(uri), None),
                Err(error) => (
                    None,
                    Some(format!(
                        "\n[Complete tool evidence could not be retained: {error:#}]"
                    )),
                ),
            },
            None => (None, None),
        };
        let limit = crate::agent::workflow::efficiency::preview_limit(
            &self.config.efficiency,
            self.context_window(),
            &call.name,
            output.is_error,
            artifact.is_some(),
        );
        let limit = budget.limit(
            call,
            &output,
            limit,
            artifact.is_some(),
            duplicate.is_some(),
            pending,
        );
        let edit = crate::agent::workflow::efficiency::edit_result(
            &call.name,
            &output,
            artifact.is_some(),
        );
        let (mut body, dropped) = crate::agent::workflow::efficiency::visible_result(
            edit.as_deref().unwrap_or(&output.text),
            limit,
            duplicate,
        );
        let dropped = dropped || edit.is_some();
        let mut presentation_changed = body != output.text;
        if let Some(compact) =
            crate::agent::workflow::efficiency::compact_command(call, &output, artifact.as_deref())
        {
            body = compact;
            presentation_changed = true;
        }
        if presentation_changed {
            let integrity = output.execution.output.get_or_insert_with(Default::default);
            integrity.mark_presentation_partial();
        }
        crate::telemetry::tool_output(
            self.config,
            self.hermetic,
            self.session.id(),
            &call.name,
            tool_seconds,
            status,
            &output,
        );
        if let Some(uri) = &artifact {
            crate::agent::workflow::efficiency::evidence_link(&mut body, uri, dropped);
        }
        if let Some(notice) = &retain_notice {
            body.push_str(notice);
        }
        if let Some(note) = &search_note {
            body.push_str(note);
        }
        if let Some(note) = repro_advice {
            body.push_str(note);
        }
        budget.spend(body.len());
        self.efficiency_metrics.result(&output.text, &body);
        let mut message = llm::tool_message(&call.id, &body);
        crate::tools::surface::outcome::annotate(&mut message, &output);
        crate::agent::compact::snapcompact::views::mark(&mut message, call, &output);
        message["dial_evidence"] = json!({"artifact":artifact,"is_error":output.is_error});
        message["is_error"] = json!(output.is_error);
        if call.name == "ask"
            && let Some(data) = &output.data
        {
            message["dial_question_result"] = data.clone();
        }
        if call.name == "task"
            && let Some(note) = crate::agent::workers::subagents::structured_result_note(&output)
        {
            turn.pending_nudges.push(note);
        }
        let clipped = output.text.len() > limit
            || output.text.contains("output omitted")
            || output.text.contains("bytes clipped");
        if clipped {
            let mut state = self.session.control_state();
            let mut uris = state["truncated_artifacts"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let before = uris.len();
            for uri in artifact
                .iter()
                .map(String::as_str)
                .chain(crate::storage::artifact::uris(&output.text))
            {
                if !uris.contains(&json!(uri)) {
                    uris.push(json!(uri));
                }
            }
            // Only a recovery metric reads this list. Unbounded, every update journaled the whole
            // array again (quadratic journal growth over a long session); keep the newest entries
            // and write nothing when no new artifact appeared.
            if uris.len() != before {
                if uris.len() > TRUNCATED_ARTIFACTS_KEPT {
                    uris.drain(..uris.len() - TRUNCATED_ARTIFACTS_KEPT);
                }
                state["truncated_artifacts"] = json!(uris);
                self.session.set_control_state(state)?;
            }
        }
        if call.name == "read" && !output.is_error && !is_duplicate {
            let mut observation = call.arguments.clone();
            if let Some(args) = observation.as_object_mut() {
                args.remove("i");
            }
            message["dial_evidence"]["observation"] = json!(observation.to_string());
        }
        if let Some(image) = image {
            message["dial_images"] = json!([image]);
        }
        if matches!(call.name.as_str(), "ask" | "eval")
            && let Some(images) = output
                .data
                .as_ref()
                .and_then(|data| data["dial_images"].as_array())
            && !images.is_empty()
        {
            message["dial_images"] = json!(images);
        }
        crate::agent::workflow::questions::project_callbacks(
            &mut message,
            std::mem::take(&mut self.callback_question_results),
            limit,
        );
        // A read image, or the file `image_gen` just wrote, is attached so a vision model can
        // inspect it (and regenerate when it misses the request).
        let attach = match call.name.as_str() {
            "read" => call.arguments["path"].as_str(),
            crate::tools::net::image_gen::TOOL => call.arguments["output"].as_str(),
            _ => None,
        };
        if !output.is_error
            && !self.hermetic
            && let Some(path) = attach
        {
            let path = Path::new(path);
            if llm::images::is_image(path) {
                let authorized = (call.name == "read")
                    .then(|| llm::images::authorized_user_reference(&self.session.messages(), path))
                    .flatten();
                let attachment = if let Some(image) = authorized {
                    Ok(image)
                } else {
                    self.workspace
                        .resolve(path.to_string_lossy().as_ref())
                        .map_err(anyhow::Error::msg)
                        .and_then(|absolute| {
                            let relative = absolute
                                .strip_prefix(self.workspace.root())
                                .context("image resolves outside the project")?;
                            llm::images::attachment(self.workspace.root(), relative)
                        })
                };
                match attachment {
                    Ok(image) => message["dial_images"] = json!([image]),
                    Err(error) => ui.notice(&format!("Image attachment unavailable: {error:#}")),
                }
            }
        }
        if !self.hermetic && message.get("dial_images").is_some() && backend.accepts_image_input() {
            self.step_checks.visual.capture_output(
                &output,
                self.step_checks.revision(),
                &message["dial_images"],
            );
        }
        for reference in message["dial_images"].as_array().into_iter().flatten() {
            ui.image(reference);
        }
        if crate::agent::workflow::exploration::TOOL_NAMES.contains(&call.name.as_str())
            && !output.is_error
        {
            message["dial_exploration_result"] = json!(true);
        }
        if !self.hermetic && !output.is_error {
            let model = crate::agent::window::file_handles::model_result(
                &self.workspace,
                call,
                message["content"].as_str().unwrap_or(""),
            );
            if message["content"].as_str() != Some(model.as_str()) {
                message["dial_model_content"] = json!(model);
            }
            // Journal the registry only when it grew; an unchanged snapshot would still
            // append a state entry after every tool result.
            let snapshot = serde_json::to_value(self.workspace.file_handle_snapshot())?;
            let mut control = self.session.control_state();
            if control["file_handles"] != snapshot {
                control["file_handles"] = snapshot;
                self.session.set_control_state(control)?;
            }
        }
        self.push(message)?;
        // A changed history snapshot may make a previously dead-ended compaction fit.
        turn.compaction_failed = false;
        let (tokens, estimated) = self.context_usage();
        ui.context_usage(tokens, estimated);
        Ok(())
    }
}

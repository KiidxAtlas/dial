//! The `Ui` implementation for `Bridge`.

use crate::agent::workflow::todo::TodoList;
use crate::agent::{self, Decision, Ui};
use crate::app::bridge::Bridge;
use crate::app::notices::RetryNotice;
use crate::app::shell::TICK;
use crate::runtime::tool_output::ToolOutput;
use crate::ui::composer::SteerStage;
use crate::ui::status::Phase;
use crate::ui::theme::Tok;
use crate::ui::{tasks, text};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Ignore result-only policy updates: only changed request controls warrant a transcript notice.
fn request_settings_change(
    status: &mut crate::ui::status::StatusData,
    previous: &Value,
    control: &Value,
) -> Option<String> {
    let policy = &control["adaptive_policy"];
    let settings = &policy["selection"]["effective"];
    status.inference = crate::adaptive::render_request_settings(settings, true).unwrap_or_default();
    let old = &previous["adaptive_policy"];
    if old["model"] == policy["model"] && old["selection"]["effective"] == *settings {
        return None;
    }
    crate::adaptive::render_request_settings(settings, false)
}

impl Ui for Bridge<'_> {
    fn renders_workers(&self) -> bool {
        true
    }

    fn runtime(&mut self, tree: &crate::runtime::Tree) {
        self.shell.runtime = tree.clone();
    }

    fn result_node(&mut self, body: &crate::runtime::Body) {
        let (tool, prose, running) = body.read(|n| {
            (
                n.kind == "tool",
                matches!(n.kind.as_str(), "assistant" | "reasoning"),
                n.result.phase == crate::runtime::Phase::Running,
            )
        });
        if tool {
            self.shell.pending_result = running.then(|| body.clone());
        } else if prose {
            self.shell.transcript.bind_prose(body);
            self.shell.shared_prose = running;
        }
    }

    fn maintenance_ready(&mut self) -> bool {
        self.pump();
        self.shell.startup.is_none()
            && self.shell.deferred.is_empty()
            && !self.shell.composer.has_queued_work()
            && self.shell.pending_config.is_none()
            && !self.shell.pending_resource_rebuild
    }

    fn take_pending_messages(&mut self) -> Vec<String> {
        self.pump();
        self.shell.composer.take_steering()
    }

    fn restore_pending_messages(&mut self, messages: Vec<String>) {
        self.shell.composer.restore_pending_messages(messages);
        self.repaint(true);
    }

    /// Explicit "Steer now" input. Taking it is what lets the agent cancel a stale model reply and
    /// interrupt only waits that are safe to interrupt; running effects are never torn down here.
    fn take_steering(&mut self) -> Vec<String> {
        self.pump();
        let taken = self.shell.composer.take_explicit_steering();
        for text in &taken {
            self.shell
                .composer
                .mark_explicit(text, SteerStage::Received);
        }
        if !taken.is_empty() {
            self.repaint(true);
        }
        taken
    }

    fn take_independent_messages(&mut self) -> Vec<String> {
        self.pump();
        let taken = self.shell.composer.take_explicit_independent();
        for text in &taken {
            self.shell
                .composer
                .mark_explicit(text, SteerStage::Received);
        }
        taken
    }

    fn defer_pending_messages(&mut self, messages: Vec<String>) {
        self.shell.composer.defer_messages(messages);
        self.repaint(true);
    }

    fn steering_applied(&mut self, text: &str) {
        // Automatically routed input is shown by `pending_message_accepted`; only explicit here.
        if self.shell.composer.mark_explicit(text, SteerStage::Applied) {
            self.shell.transcript.push_user(text);
            self.repaint(true);
        }
    }

    fn pending_message_accepted(&mut self, text: &str) {
        self.shell.composer.mark_explicit(text, SteerStage::Applied);
        self.shell.transcript.push_user(text);
        self.repaint(true);
    }

    fn context_usage(&mut self, tokens: u64, estimated: bool) {
        self.shell.status.prompt_tokens = Some(tokens);
        self.shell.status.context_estimated = estimated;
        self.repaint(false);
    }

    fn usage(&mut self, _prompt: Option<u64>, output: Option<u64>, seconds: f64) {
        let status = &mut self.shell.status;
        let generation_rate = status
            .model_progress
            .take()
            .filter(|p| !p.estimated)
            .and_then(|p| p.tokens_per_second);
        status.model_silence_secs = 0;
        if let Some(tokens) = output {
            self.shell.turn_output_tokens = self.shell.turn_output_tokens.saturating_add(tokens);
            status.output_tokens = self.shell.turn_output_tokens;
            status.tokens_per_second = generation_rate
                .or_else(|| (seconds > 0.0 && tokens > 0).then_some(tokens as f64 / seconds));
        }
        self.repaint(true);
    }

    fn model_progress(&mut self, progress: &crate::llm::ModelProgress) {
        if self.shell.request_phase == agent::RequestPhase::Cancelling {
            return; // Ignore stale worker telemetry while cancellation settles.
        }
        self.shell.status.model_progress = Some(progress.clone());
        self.shell.status.model_silence_secs = 0;
        self.repaint(false);
    }

    fn reasoning_delta(&mut self, text: &str) {
        if !std::mem::take(&mut self.shell.shared_prose) {
            self.shell.transcript.thinking_delta(text);
        }
        self.set_activity("reasoning");
        self.repaint(false);
    }

    fn tool_arguments_delta(&mut self, _index: u64, _id: &str, name: &str, _delta: &str) {
        self.set_activity(&if name.is_empty() {
            "preparing tool call".into()
        } else {
            format!("preparing {name}")
        });
        self.repaint(false);
    }

    fn tool_output(&mut self, name: &str, text: &str) {
        if let Some((id, _, _)) = self.pending.iter().rev().find(|(_, tool, _)| tool == name) {
            self.shell.transcript.tool_delta(id, text);
        }
        self.repaint(false);
    }

    fn tool_progress(&mut self, name: &str, text: &str) {
        self.shell.transcript.tool_progress(name, text);
        self.repaint(false);
    }

    fn background_output(&mut self, name: &str, text: &str, replace: bool) {
        self.shell.transcript.background_output(name, text, replace);
        self.repaint(false);
    }

    fn background_notice(&mut self, message: &str) {
        self.shell.transcript.notice("Background", message);
        self.repaint(true);
    }

    fn event(&mut self, event: &crate::agent::ui::ui_event::UiEvent) {
        self.shell.transcript.event(event);
        self.repaint(true);
    }

    /// Cache hits are bookkeeping, not conversation: usage accounting keeps the counts for `/cost`,
    /// and the transcript stays about the work.
    fn cached_usage(&mut self, _read: Option<u64>, _write: Option<u64>) {}

    fn extension_panel(&mut self, title: &str, markdown: &str) {
        self.shell.transcript.notice(title, "");
        self.shell.transcript.assistant_delta(markdown);
        self.shell.transcript.finish_turn();
        self.repaint(true);
    }
    fn review_turn_draft(&mut self, drafted: &str, cancel: &AtomicBool) -> bool {
        self.shell.review_turn_draft(cancel, |shell| {
            shell.ask_with_cancel(
                "Turn drafter · review before sending",
                &format!("{drafted}\n\nAccept sends this instruction to the agent. Accept all draft messages also accepts pending and future drafts without review until Dial exits. Cancel sends nothing. Use Page Up/Page Down to review the full draft."),
                &[
                    ("Accept and send", 'a'),
                    ("Accept all draft messages", 's'),
                    ("Cancel", 'c'),
                ],
                Some(cancel),
            )
        })
    }

    fn role(&mut self, role: &str, model: &str) {
        let status = &mut self.shell.status;
        status.model_progress = None;
        status.model_silence_secs = 0;
        status.role = if role == "main" {
            String::new()
        } else {
            role.to_owned()
        };
        if let Some((provider, name)) = model.split_once('/') {
            status.provider = provider.to_owned();
            status.model = name.to_owned();
        }
        self.repaint(true);
    }

    fn text_delta(&mut self, text: &str) {
        if !std::mem::take(&mut self.shell.shared_prose) {
            self.shell.transcript.assistant_delta(text);
        }
        self.set_activity("writing");
        self.repaint(false);
    }

    fn tool_call(&mut self, name: &str, summary: &str) {
        self.shell.status.model_progress = None;
        self.next_id += 1;
        let id = format!("call-{}", self.next_id);
        self.shell
            .transcript
            .tool_start(&id, name, summary, summary);
        if let Some(body) = self.shell.pending_result.take() {
            body.correlate(&id);
            self.shell.transcript.bind_tool(&id, body);
        }
        self.pending.push((id, name.to_owned(), Instant::now()));
        self.set_activity(&format!("running {name}"));
        self.repaint(true);
    }

    fn tool_result(&mut self, name: &str, output: &ToolOutput) {
        let at = self.pending.iter().rposition(|(_, n, _)| n == name);
        let (id, started) = match at {
            Some(i) => {
                let (id, _, started) = self.pending.remove(i);
                (id, started)
            }
            None => {
                self.next_id += 1;
                (format!("call-{}", self.next_id), Instant::now())
            }
        };
        self.shell.transcript.tool_end(
            &id,
            &output.text,
            output.is_error,
            started.elapsed().as_secs_f64(),
        );
        self.attach_write_preview(&id, output);
        let activity = self.pending.last().map_or_else(
            || "thinking".to_owned(),
            |(_, name, _)| format!("running {name}"),
        );
        self.set_activity(&activity);
        self.repaint(true);
    }

    fn tool_call_with_id(&mut self, id: &str, name: &str, summary: &str) {
        self.shell.status.model_progress = None;
        self.shell.transcript.tool_start(id, name, summary, summary);
        if let Some(body) = self.shell.pending_result.take() {
            self.shell.transcript.bind_tool(id, body);
        }
        self.pending
            .push((id.to_owned(), name.to_owned(), Instant::now()));
        self.set_activity(&format!("running {name}"));
        self.repaint(true);
    }

    fn tool_call_with_args(&mut self, id: &str, name: &str, summary: &str, args: &Value) {
        self.tool_call_with_id(id, name, summary);
        self.shell.transcript.tool_input(id, args.to_string());
    }

    fn tool_presentation(
        &mut self,
        id: &str,
        view: &crate::tools::surface::presentation::Presentation,
    ) {
        self.shell.transcript.tool_presentation(id, view);
    }

    fn tool_result_with_id(&mut self, id: &str, _name: &str, output: &ToolOutput) {
        let started = self
            .pending
            .iter()
            .position(|(pending_id, _, _)| pending_id == id)
            .map(|index| self.pending.remove(index).2)
            .unwrap_or_else(Instant::now);
        self.shell.transcript.tool_end(
            id,
            &output.text,
            output.is_error,
            started.elapsed().as_secs_f64(),
        );
        if let Some(data) = &output.data {
            self.shell.transcript.tool_data(id, data);
        }
        self.attach_write_preview(id, output);
        if let Err(error) =
            crate::cli::watch::tool_result(&self.app.layout, &self.shell.session_id, _name, output)
        {
            self.shell
                .note("Live feed", format!("Could not record edit: {error:#}"));
        }
        let activity = self.pending.last().map_or_else(
            || "thinking".to_owned(),
            |(_, name, _)| format!("running {name}"),
        );
        self.set_activity(&activity);
        self.repaint(true);
    }

    fn request_policy(&mut self, control: &Value) {
        let notice = if control["adaptive_policy"]["model"].as_str()
            == Some(self.app.config.model.as_str())
        {
            request_settings_change(&mut self.shell.status, &self.shell.request_policy, control)
        } else {
            None
        };
        self.shell.request_policy = control.clone();
        if let Some(notice) = notice {
            self.shell.note("Model settings", notice);
            self.repaint(true);
        }
    }

    fn notice(&mut self, message: &str) {
        // Provider retries arrive once per attempt. The first one is explained in plain words;
        // later attempts stay off the transcript (the status line already shows the wait).
        if let Some(retry) = RetryNotice::parse(message) {
            if !std::mem::replace(&mut self.shell.retry_noted, true) {
                self.shell.note(
                    "Retrying",
                    format!(
                        "The model request failed ({}). Dial retries up to {} times with growing waits; Esc stops.",
                        retry.cause, retry.max
                    ),
                );
            }
            self.repaint(true);
            return;
        }
        self.shell.note("Notice", message);
        self.repaint(true);
    }

    fn changed_files(&mut self, _paths: &[String]) {
        // The completion footer owns changed-file counts and review/undo actions.
    }

    /// Policy and index bookkeeping stays out of the transcript; `/policy` shows the same data on demand.
    fn diagnostic(&mut self, _message: &str) {}

    fn image(&mut self, reference: &Value) {
        if self.interrupt.load(Ordering::SeqCst) {
            return;
        }
        let reference = reference.clone();
        let native = self.shell.term.image_protocol().is_some();
        let layout = self.app.layout.clone();
        let session = self.shell.session_id.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("dial-image-preview".into())
            .spawn(move || {
                let _ = tx.send(crate::cli::watch::preview_image(
                    &layout, &session, &reference, native,
                ));
            });
        if worker.is_err() {
            return;
        }
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Ok((thumbnail, warning))) => {
                    if let Some(warning) = warning {
                        self.shell.note("Live feed", warning);
                    }
                    if let Some(thumbnail) = thumbnail {
                        self.shell.transcript.image(thumbnail);
                    } else {
                        self.shell
                            .note("Image", "Screenshot sent to the watch pane.");
                    }
                    self.repaint(true);
                    break;
                }
                Ok(Err(error)) => {
                    self.notice(&format!("Terminal image preview unavailable: {error:#}"));
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    self.heartbeat();
                    if self.interrupt.load(Ordering::SeqCst) {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn laya_activity(&mut self, status: &str, message: &str) {
        if matches!(status, "error" | "failed" | "unavailable") {
            self.shell.note("Decision advice", message);
        }
        self.set_activity(if status == "checking" {
            "Laya checking"
        } else if status == "loading" {
            "Laya loading"
        } else {
            "thinking"
        });
        self.repaint(true);
    }

    fn todo(&mut self, todo: &TodoList) {
        if let Some((id, _, _)) = self
            .pending
            .iter()
            .rev()
            .find(|(_, name, _)| name == "todo")
        {
            self.shell
                .transcript
                .tool_todo(id, tasks::changes(&self.shell.todo, todo));
        }
        self.shell.todo = todo.clone();
        self.repaint(true);
    }

    fn heartbeat(&mut self) {
        self.repaint(false);
    }

    /// The turn's outcome joins its closing rule in the transcript; changed files are already in
    /// the session rail and `/review-file` lists them.
    fn completion(&mut self, summary: &Value) {
        self.shell
            .transcript
            .completion(crate::ui::transcript::TurnOutcome {
                status: summary["outcome"].as_str().unwrap_or("unknown").to_owned(),
                changed_files: summary["changed_files"].as_array().map_or(0, Vec::len),
                open_tasks: summary["open_tasks"].as_u64().unwrap_or(0) as usize,
            });
        self.repaint(true);
    }

    fn request_phase(&mut self, phase: agent::RequestPhase) {
        let changed = phase != self.shell.request_phase;
        self.shell.request_phase = phase;
        if matches!(
            phase,
            agent::RequestPhase::Waiting
                | agent::RequestPhase::Retrying
                | agent::RequestPhase::Cancelling
        ) {
            self.shell.status.model_progress = None;
        }
        if changed && phase == agent::RequestPhase::Waiting {
            self.shell.transcript.end_text();
        }
        if changed || phase == agent::RequestPhase::Waiting {
            self.set_activity(phase.label());
            self.repaint(true);
        }
    }

    fn model_wait(&mut self, silence: Duration) {
        if self.shell.status.model_progress.is_some() {
            self.shell.status.model_silence_secs = silence.as_secs();
            return;
        }
        if silence.is_zero() {
            self.set_activity(self.shell.request_phase.label());
        } else if silence >= Duration::from_secs(15) {
            self.set_activity(&format!(
                "{} · no progress for {}s",
                self.shell.request_phase.label(),
                silence.as_secs()
            ));
        }
    }

    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        self.approve_with_cancel(tool, summary, &std::sync::atomic::AtomicBool::new(false))
    }

    fn approve_with_cancel(
        &mut self,
        tool: &str,
        summary: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Decision {
        self.shell.status.phase = Phase::Working;
        // Capability expansion never offers session/project-wide approval shortcuts.
        if matches!(tool, "request_directory" | "request_sensitive_directory") {
            let choice = self.shell.ask_with_cancel(
                &format!("Allow {tool}?"),
                summary,
                &[("Yes — allow this directory", 'y'), ("No — deny", 'n')],
                Some(cancel),
            );
            self.repaint(true);
            return if choice == Some(0) {
                Decision::Allow
            } else {
                Decision::Deny
            };
        }
        let choice = self.shell.ask_with_cancel(
            &format!("Allow {tool}?"),
            summary,
            &[
                ("Allow once", 'y'),
                ("Allow for this session", 'a'),
                ("Allow for this project", 'p'),
                ("Deny", 'n'),
            ],
            Some(cancel),
        );
        let decision = match choice {
            Some(0) => Decision::Allow,
            Some(1) => Decision::AllowAlways,
            Some(2) => Decision::AllowProject,
            _ => Decision::Deny,
        };
        self.repaint(true);
        decision
    }

    fn interactive_input(&self) -> bool {
        true
    }

    fn ask_questions(
        &mut self,
        request: &agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> agent::workflow::questions::QuestionOutcome {
        use agent::workflow::questions::QuestionOutcome;
        let mut dialog = crate::ui::questions::Dialog::new(request, &self.app.layout.project_root);
        if request.notify && self.shell.ui.notify {
            crate::ui::termcaps::notify(
                crate::ui::termcaps::NotifyKind::Question,
                "Dial",
                "A response is needed",
                &self.shell.session_id,
            );
        }
        let was_waiting = std::mem::replace(&mut self.shell.awaiting_user, true);
        let outcome = loop {
            if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
                break QuestionOutcome::Cancelled;
            }
            let (width, height) = self.shell.term.size();
            let rows = crate::ui::questions::framed(
                dialog.rows(
                    usize::from(width).saturating_sub(4),
                    usize::from(height).saturating_sub(5),
                ),
                usize::from(width),
                usize::from(height).saturating_sub(1),
                self.shell.themes.current(),
            );
            if self.shell.paint(Some(rows)).is_err() {
                break QuestionOutcome::Disconnected;
            }
            if let Err(error) = dialog.presented() {
                break QuestionOutcome::Unsupported(error);
            }
            match poll(Duration::from_millis(50)) {
                Ok(false) => {
                    if let Some(outcome) = dialog.expired() {
                        break outcome;
                    }
                    continue;
                }
                Err(_) => break QuestionOutcome::Disconnected,
                Ok(true) => {}
            }
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if let Some(outcome) = dialog.key(key, &self.shell.keys) {
                        break outcome;
                    }
                }
                Ok(Event::Paste(value)) => dialog.paste(&value),
                Ok(Event::Resize(..)) => self.shell.term.on_resize(),
                Ok(_) => {}
                Err(_) => break QuestionOutcome::Disconnected,
            }
        };
        self.shell.awaiting_user = was_waiting;
        self.repaint(true);
        outcome
    }

    fn ask_user(&mut self, title: &str, question: &str, options: &[String]) -> Option<String> {
        let mut choices: Vec<(String, char)> = options
            .iter()
            .enumerate()
            .map(|(index, option)| (option.clone(), char::from(b'1'.wrapping_add(index as u8))))
            .collect();
        choices.push(("Other (type your own)".into(), 'o'));
        let choice_refs: Vec<(&str, char)> = choices
            .iter()
            .map(|(label, key)| (label.as_str(), *key))
            .collect();

        let was_waiting = std::mem::replace(&mut self.shell.awaiting_user, true);
        let answer = match self.shell.ask(title, question, &choice_refs) {
            Some(index) if index < options.len() => Some(options[index].clone()),
            Some(_) => {
                let mut editor = crate::ui::editor::Editor::default();
                let prompt = format!("{} (Enter sends, Esc cancels)", text::one_line(question));
                let mut answer = None;
                loop {
                    let (width, _) = self.shell.term.size();
                    let theme = self.shell.themes.current();
                    let rows = vec![
                        Line::from(Span::styled(
                            text::clip(&prompt, width as usize),
                            theme.fg(Tok::Warning),
                        )),
                        Line::from(text::clip(&text::one_line(&editor.text), width as usize)),
                    ];
                    if self.shell.paint(Some(rows)).is_err() {
                        break;
                    }
                    match read() {
                        Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                            match key.code {
                                KeyCode::Enter => {
                                    if !editor.text.trim().is_empty() {
                                        answer = Some(editor.text);
                                        break;
                                    }
                                }
                                KeyCode::Esc => break,
                                KeyCode::Char('c')
                                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                {
                                    break;
                                }
                                _ => {
                                    editor.key(key);
                                }
                            }
                        }
                        Ok(Event::Paste(value)) => editor.insert(&text::one_line(&value)),
                        Ok(Event::Resize(..)) => self.shell.term.on_resize(),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                answer
            }
            None => None,
        };
        self.shell.awaiting_user = was_waiting;
        self.repaint(true);
        answer
    }

    fn approval_reason(&mut self) -> Option<String> {
        let mut editor = crate::ui::editor::Editor::default();
        let was_waiting = std::mem::replace(&mut self.shell.awaiting_user, true);
        let reason = loop {
            let (width, _) = self.shell.term.size();
            let theme = self.shell.themes.current();
            let rows = vec![
                Line::from(Span::styled(
                    text::clip(
                        "Plan denied: feedback (Enter sends, Esc skips)",
                        width as usize,
                    ),
                    theme.fg(Tok::Warning),
                )),
                Line::from(text::clip(&text::one_line(&editor.text), width as usize)),
            ];
            if self.shell.paint(Some(rows)).is_err() {
                break None;
            }
            let Ok(event) = read() else {
                break None;
            };
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => {
                        break (!editor.text.trim().is_empty()).then_some(editor.text);
                    }
                    KeyCode::Esc => break None,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break None;
                    }
                    _ => {
                        editor.key(key);
                    }
                },
                Event::Paste(value) => editor.insert(&text::one_line(&value)),
                Event::Resize(..) => self.shell.term.on_resize(),
                _ => {}
            }
        };
        self.shell.awaiting_user = was_waiting;
        reason
    }
}

#[cfg(test)]
mod request_settings_tests;

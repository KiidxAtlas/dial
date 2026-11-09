//! The `Ui` implementation for `Bridge`.

use crate::agent::workflow::todo::TodoList;
use crate::agent::{self, Decision, Ui};
use crate::app::bridge::Bridge;
use crate::app::notices::RetryNotice;
use crate::app::shell::TICK;
use crate::app::shell::prompts::{
    ATTENTION_HINT, AttentionExit, TypeAhead, attention_key, stop_key,
};
use crate::runtime::tool_output::ToolOutput;
use crate::ui::composer::SteerStage;
use crate::ui::input::{poll, read};
use crate::ui::keys::Action;
use crate::ui::status::Phase;
use crate::ui::theme::Tok;
use crate::ui::{tasks, text};
use crossterm::event::{Event, KeyCode, KeyEventKind};
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

struct ApprovalPrompt {
    body: String,
    options: Vec<(&'static str, char, Decision)>,
    /// What a reusable grant would match, without the lifetime sentence.
    scope: Option<String>,
}

/// How to leave an approval: shown under the action so the difference is never a surprise.
fn deny_hint(keys: &crate::ui::keys::Keymap) -> String {
    format!(
        "Esc or n denies · {} denies and stops the turn",
        keys.label(Action::Interrupt)
    )
}

/// The policy owner ends each scope with its lifetime; the option label carries that instead.
fn grant_matcher(scope: &str) -> &str {
    scope.split(" Lifetime:").next().unwrap_or(scope).trim()
}

/// Choices reflect only scopes the policy owner can actually store. The action leads the body;
/// lifetimes live in the option labels, so the preview is never pushed off-screen.
fn approval_prompt(
    summary: &str,
    preview: &agent::safety::approval::ApprovalPreview,
    hint: &str,
) -> ApprovalPrompt {
    let mut body = summary.trim_end().to_owned();
    let scope = preview
        .session
        .as_deref()
        .or(preview.project.as_deref())
        .map(|scope| grant_matcher(scope).to_owned());
    if let Some(scope) = &scope {
        body.push_str(&format!("\n\nA reusable approval would cover: {scope}"));
    }
    if let Some(reason) = &preview.unavailable_reason {
        body.push_str(&format!("\n\nOne-time approval only: {reason}"));
    }
    body.push_str(&format!("\n\n{hint}"));
    let mut options = vec![("Allow once", 'y', Decision::Allow)];
    if preview.session.is_some() {
        options.push((
            "Allow for this session (not saved)",
            'a',
            Decision::AllowAlways,
        ));
    }
    if preview.project.is_some() {
        options.push((
            "Allow for this project (saved until revoked)",
            'p',
            Decision::AllowProject,
        ));
    }
    options.push(("Deny", 'n', Decision::Deny));
    ApprovalPrompt {
        body,
        options,
        scope,
    }
}

/// One row per change, then how approving and leaving work. Contents never appear here.
fn batch_prompt(rows: &[String], hint: &str) -> String {
    format!(
        "{} changes are ready:\n\n{}\n\nEach approval applies once to that exact change; nothing is saved, and changes can partly succeed.\n\n{hint}",
        rows.len(),
        rows.iter()
            .map(|row| format!("• {row}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The reach of a saved project grant: what it matches, where it applies, how long it lasts,
/// what still asks and where to undo it.
fn project_grant_body(scope: Option<&str>, project_root: &std::path::Path) -> String {
    format!(
        "Covers: {}\nApplies to: every future session in {}\nLasts: until you revoke it in /permissions\n\nCalls that do not match still ask first. Deny rules still win.",
        scope.unwrap_or("calls matching this one"),
        project_root.display()
    )
}

/// `ask_user` hotkeys: 1-9 only; later options are reached with the arrows.
fn ask_user_hotkey(index: usize) -> char {
    u8::try_from(index)
        .ok()
        .filter(|index| *index < 9)
        .map_or(' ', |index| char::from(b'1' + index))
}

/// User-facing wording for a policy denial. The model-facing text stays in the tool result.
fn policy_notice(
    tool: &str,
    explanation: &agent::safety::approval::PolicyExplanation,
) -> Option<String> {
    use agent::safety::approval::PolicyReason;
    let scope = explanation
        .rule_scope
        .as_deref()
        .map(|scope| format!(" ({scope})"))
        .unwrap_or_default();
    Some(match explanation.reason {
        PolicyReason::DenyRule => format!(
            "{tool} was blocked by a deny rule{scope}. Deny rules win over approvals; change it in /permissions."
        ),
        PolicyReason::ReadOnly => format!(
            "{tool} was blocked: the approval mode is read-only, so nothing that changes files or runs commands is allowed."
        ),
        PolicyReason::PlanMode => format!(
            "{tool} was blocked: plan mode only investigates. Approve a plan to start making changes."
        ),
        PolicyReason::HookVeto => format!("{tool} was blocked by a pre-tool hook{scope}."),
        _ => return None,
    })
}

impl Bridge<'_> {
    /// After a prompt closes: show the stop at once when the prompt's stop key raised it.
    fn settle_prompt(&mut self, was_stopping: bool) {
        if !was_stopping && self.interrupt.load(Ordering::SeqCst) {
            self.show_stopping(None);
        } else {
            self.repaint(true);
        }
    }

    /// Saving a grant for every future session gets its own confirmation.
    fn confirm_project_grant(
        &mut self,
        tool: &str,
        scope: Option<&str>,
        cancel: &AtomicBool,
    ) -> bool {
        let body = project_grant_body(scope, &self.app.layout.project_root);
        self.shell.ask_with_cancel(
            &format!("Save {tool} permission for this project?"),
            &body,
            &[("Save for this project", 'p'), ("Back to the choices", 'b')],
            Some(cancel),
        ) == Some(0)
    }

    /// A one-line answer editor for in-turn prompts. Esc cancels; the stop key also stops the
    /// turn; Enter is ignored briefly after it appears so type-ahead cannot send it.
    fn line_prompt(&mut self, prompt: &str, title: &str, body: &str) -> Option<String> {
        let interrupt = self.interrupt.clone();
        if self.shell.park_pending_input() {
            interrupt.store(true, Ordering::SeqCst);
            return None;
        }
        let mut editor = crate::ui::editor::Editor::default();
        let mut guard = TypeAhead::default();
        loop {
            if interrupt.load(Ordering::SeqCst) {
                return None;
            }
            let (width, _) = self.shell.term.size();
            let theme = self.shell.themes.current();
            let rows = vec![
                Line::from(Span::styled(
                    text::clip(prompt, width as usize),
                    theme.fg(Tok::Warning),
                )),
                Line::from(text::clip(&text::one_line(&editor.text), width as usize)),
                Line::from(text::clip(ATTENTION_HINT, width as usize)),
            ];
            if self.shell.paint(Some(rows)).is_err() {
                return None;
            }
            guard.painted(Instant::now());
            match poll(TICK) {
                Ok(false) => continue,
                Err(_) => return None,
                Ok(true) => {}
            }
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if attention_key(&key) {
                        match self.shell.inspect_modal_attention(title, body, || {
                            interrupt.load(Ordering::SeqCst)
                        }) {
                            AttentionExit::Returned => continue,
                            AttentionExit::Cancelled | AttentionExit::Disconnected => return None,
                        }
                    }
                    if stop_key(&self.shell.keys, &key) {
                        interrupt.store(true, Ordering::SeqCst);
                        return None;
                    }
                    match key.code {
                        KeyCode::Enter if guard.blocks(&key, Instant::now()) => {}
                        KeyCode::Enter => {
                            if !editor.text.trim().is_empty() {
                                return Some(editor.text);
                            }
                        }
                        KeyCode::Esc => return None,
                        _ => {
                            editor.key_with(key, &self.shell.keys);
                        }
                    }
                }
                Ok(Event::Paste(value)) => editor.insert(&text::one_line(&value)),
                Ok(Event::Resize(..)) => self.shell.term.on_resize(),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
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
            self.shell
                .transcript
                .bind_prose_at(body, &crate::storage::session::timestamp_now());
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

    fn pending_input(&mut self) -> bool {
        self.pump();
        self.shell.composer.has_pending_input()
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
            self.shell
                .transcript
                .push_user_at(text, &crate::storage::session::timestamp_now());
            self.repaint(true);
        }
    }

    fn pending_message_accepted(&mut self, text: &str) {
        self.shell.composer.mark_explicit(text, SteerStage::Applied);
        self.shell
            .transcript
            .push_user_at(text, &crate::storage::session::timestamp_now());
        self.repaint(true);
    }

    fn context_usage(&mut self, tokens: u64, estimated: bool) {
        self.shell.status.prompt_tokens = Some(tokens);
        self.shell.status.context_estimated = estimated;
        self.repaint(false);
    }

    fn usage(&mut self, _prompt: Option<u64>, output: Option<u64>, seconds: f64) {
        // A request that needed provider retries closes its "Retrying" note with one short line.
        if let Some(attempt) = self.shell.retry_attempt.take() {
            self.shell
                .note("Recovered", RetryNotice::recovered_text(attempt));
        }
        let status = &mut self.shell.status;
        let generation_rate = status
            .model_progress
            .take()
            .filter(|p| !p.estimated)
            .and_then(|p| p.tokens_per_second);
        status.model_silence_secs = 0;
        status.provider_retry = None;
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
        self.shell.status.provider_retry = None;
        self.repaint(false);
    }

    fn reasoning_delta(&mut self, text: &str) {
        if !std::mem::take(&mut self.shell.shared_prose) {
            self.shell.transcript.thinking_delta(text);
        }
        self.set_activity("reasoning");
        self.repaint(false);
    }

    fn tool_arguments_delta(&mut self, _index: u64, id: &str, name: &str, delta: &str) {
        // Observational only: the card shows the arguments as they stream; nothing is executed
        // until the completed call starts.
        self.shell.transcript.tool_arguments_delta(id, name, delta);
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
        // Progress names its tool, not its call: it belongs to the newest pending call of that tool,
        // the same call `tool_output` streams into.
        if let Some((id, _, _)) = self.pending.iter().rev().find(|(_, tool, _)| tool == name) {
            self.shell.transcript.tool_progress(id, text);
        }
        self.repaint(false);
    }

    fn background_output(&mut self, name: &str, text: &str, replace: bool) {
        self.shell.transcript.background_output(name, text, replace);
        self.repaint(false);
    }

    fn background_notice(&mut self, message: &str) {
        self.shell.note("Background", message);
        self.repaint(true);
    }

    fn event(&mut self, event: &crate::agent::ui::ui_event::UiEvent) {
        use crate::agent::ui::ui_event::BackgroundWait;
        match &event.background_wait {
            Some(BackgroundWait::Started {
                jobs,
                workers,
                questions,
            }) => {
                let mut parts = Vec::new();
                if !jobs.is_empty() {
                    parts.push(format!("jobs {}", jobs.join(", ")));
                }
                if !workers.is_empty() {
                    parts.push(format!("workers {}", workers.join(", ")));
                }
                if !questions.is_empty() {
                    parts.push(format!("questions {}", questions.join(", ")));
                }
                self.shell.status.background_wait = Some(crate::ui::status::WorkWait {
                    label: parts.join("; "),
                    started: Instant::now(),
                });
                self.shell.status.model_progress = None;
                self.shell.status.model_silence_secs = 0;
            }
            Some(BackgroundWait::Ended { .. }) => {
                self.shell.status.background_wait = None;
                self.set_activity("resuming after work wait");
            }
            None => {}
        }
        self.shell.transcript.event(event);
        self.repaint(true);
    }

    /// Cache hits are bookkeeping, not conversation: usage accounting keeps the counts for `/cost`,
    /// and the transcript stays about the work.
    fn cached_usage(&mut self, _read: Option<u64>, _write: Option<u64>) {}

    fn extension_panel(&mut self, title: &str, markdown: &str) {
        self.shell.transcript.notice(title, "");
        self.shell
            .transcript
            .assistant_delta_at(markdown, &crate::storage::session::timestamp_now());
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
            self.shell
                .transcript
                .assistant_delta_at(text, &crate::storage::session::timestamp_now());
        }
        self.shell.transcript.advance_reveal(&self.shell.ui);
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
        self.shell.status.tool_started = Some(Instant::now());
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
        self.shell.transcript.tool_end_with(
            &id,
            &output.text,
            crate::ui::tool_cards::ToolOutcome::of(output),
            started.elapsed().as_secs_f64(),
        );
        self.attach_write_preview(&id, output);
        let activity = self.pending.last().map_or_else(
            || "thinking".to_owned(),
            |(_, name, _)| format!("running {name}"),
        );
        self.shell.status.tool_started = self.pending.last().map(|(_, _, started)| *started);
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
        self.shell.status.tool_started = Some(Instant::now());
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
        self.shell.transcript.tool_end_with(
            id,
            &output.text,
            crate::ui::tool_cards::ToolOutcome::of(output),
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
        self.shell.status.tool_started = self.pending.last().map(|(_, _, started)| *started);
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
            self.shell.status.provider_retry =
                Instant::now().checked_add(retry.delay).map(|ready_at| {
                    crate::ui::status::ProviderRetry {
                        attempt: retry.attempt,
                        maximum: retry.max.clone(),
                        ready_at,
                    }
                });
            self.shell.retry_attempt = Some(retry.attempt);
            if !std::mem::replace(&mut self.shell.retry_noted, true) {
                self.shell.note(
                    "Retrying",
                    format!(
                        "The model request failed ({}). Dial retries up to {} times; the status line shows each wait. /retry retries now; {} stops.",
                        retry.cause, retry.max, self.shell.keys.label(crate::ui::keys::Action::Interrupt)
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
        self.shell.transcript.advance_reveal(&self.shell.ui);
        self.repaint(false);
    }

    /// The existing report owner projects the same recorded evidence into the closing footer.
    fn completion(&mut self, summary: &Value) {
        self.shell
            .transcript
            .completion(crate::app::shell::turn_report::outcome(
                summary,
                &Value::Null,
            ));
        self.repaint(true);
    }

    fn provider_retry_control(&mut self, control: Option<crate::llm::provider::RetryControl>) {
        self.shell.retry_control = control;
    }

    fn request_phase(&mut self, phase: agent::RequestPhase) {
        let changed = phase != self.shell.request_phase;
        self.shell.request_phase = phase;
        if !matches!(
            phase,
            agent::RequestPhase::Backoff | agent::RequestPhase::Retrying
        ) {
            self.shell.status.provider_retry = None;
        }
        if matches!(
            phase,
            agent::RequestPhase::Waiting
                | agent::RequestPhase::Backoff
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

    fn policy_explanation(
        &mut self,
        _call_id: &str,
        tool: &str,
        explanation: &crate::agent::safety::approval::PolicyExplanation,
    ) {
        match policy_notice(tool, explanation) {
            Some(notice) => self.notice(&notice),
            None => self.diagnostic(&format!("{tool}: {explanation}")),
        }
    }
    /// Legacy prompts (the plan review among them) still stop the turn on the stop key.
    fn approve(&mut self, tool: &str, summary: &str) -> Decision {
        let interrupt = self.interrupt.clone();
        self.approve_with_cancel(tool, summary, &interrupt)
    }

    fn approve_with_cancel(
        &mut self,
        tool: &str,
        summary: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Decision {
        let phase = std::mem::replace(&mut self.shell.status.phase, Phase::Working);
        let was_stopping = self.interrupt.load(Ordering::SeqCst);
        let stop = self.shell.keys.label(Action::Interrupt);
        let hint = deny_hint(&self.shell.keys);
        // Capability expansion never offers session/project-wide approval shortcuts, and a
        // direct legacy prompt has no policy-owned reusable matcher: both allow once only.
        let (title, body, options) = match tool {
            "exit_plan" => (
                crate::app::panel::PLAN_PANEL.to_owned(),
                format!(
                    "{}{}\n\nApproving leaves plan mode and starts the work; normal tool approvals still apply.\n\nEsc or n keeps planning · {stop} stops the turn",
                    summary.trim_end(),
                    crate::app::panel::MARKDOWN_END,
                ),
                [
                    ("Approve plan", 'y'),
                    ("Keep planning (give feedback)", 'n'),
                ],
            ),
            "request_directory" | "request_sensitive_directory" => (
                format!("Allow {tool}?"),
                format!(
                    "{}\n\nApplies to this request only.\n\n{hint}",
                    summary.trim_end()
                ),
                [("Allow once", 'y'), ("Deny", 'n')],
            ),
            _ => (
                format!("Allow {tool}?"),
                format!(
                    "{}\n\nApplies to this call only; nothing is saved.\n\n{hint}",
                    summary.trim_end()
                ),
                [("Allow once", 'y'), ("Deny", 'n')],
            ),
        };
        let choice = self
            .shell
            .ask_with_cancel(&title, &body, &options, Some(cancel));
        self.shell.status.phase = phase;
        self.settle_prompt(was_stopping);
        if choice == Some(0) {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }

    fn approve_call_with_preview(
        &mut self,
        _call_id: &str,
        tool: &str,
        summary: &str,
        preview: &agent::safety::approval::ApprovalPreview,
        cancel: Option<&AtomicBool>,
    ) -> Decision {
        let interrupt = self.interrupt.clone();
        let cancel = cancel.unwrap_or(&interrupt);
        let was_stopping = self.interrupt.load(Ordering::SeqCst);
        let prompt = approval_prompt(summary, preview, &deny_hint(&self.shell.keys));
        let choices: Vec<_> = prompt
            .options
            .iter()
            .map(|(label, key, _)| (*label, *key))
            .collect();
        let decision = loop {
            let decision = self
                .shell
                .ask_with_cancel(
                    &format!("Allow {tool}?"),
                    &prompt.body,
                    &choices,
                    Some(cancel),
                )
                .and_then(|index| prompt.options.get(index))
                .map_or(Decision::Deny, |(_, _, decision)| *decision);
            if decision != Decision::AllowProject
                || self.confirm_project_grant(tool, prompt.scope.as_deref(), cancel)
            {
                break decision;
            }
            if cancel.load(Ordering::Acquire) {
                break Decision::Deny;
            }
        };
        self.settle_prompt(was_stopping);
        decision
    }

    /// One row per change (tool, target, size), then all, one by one, or none.
    fn approve_batch(&mut self, calls: &[crate::llm::ToolCall], cancel: &AtomicBool) -> Vec<bool> {
        if calls.is_empty() || cancel.load(Ordering::Acquire) {
            return vec![false; calls.len()];
        }
        let was_stopping = self.interrupt.load(Ordering::SeqCst);
        let hint = deny_hint(&self.shell.keys);
        let rows: Vec<String> = calls
            .iter()
            .map(agent::safety::approval::batch_call_line)
            .collect();
        let choice = self.shell.ask_with_cancel(
            &format!("Approve {} changes?", rows.len()),
            &batch_prompt(&rows, &hint),
            &[
                ("Approve all", 'y'),
                ("Choose one by one", 'c'),
                ("Deny all", 'n'),
            ],
            Some(cancel),
        );
        let mut allowed = match choice {
            Some(0) => vec![true; rows.len()],
            Some(1) => {
                let mut allowed = Vec::with_capacity(rows.len());
                for (index, row) in rows.iter().enumerate() {
                    let yes = !cancel.load(Ordering::Acquire)
                        && self.shell.ask_with_cancel(
                            &format!("Change {} of {}", index + 1, rows.len()),
                            &format!(
                                "{row}\n\nApplies to this change only; nothing is saved.\n\n{hint}"
                            ),
                            &[("Allow once", 'y'), ("Deny", 'n')],
                            Some(cancel),
                        ) == Some(0);
                    allowed.push(yes);
                }
                allowed
            }
            _ => vec![false; rows.len()],
        };
        if cancel.load(Ordering::Acquire) {
            allowed = vec![false; rows.len()];
        }
        self.settle_prompt(was_stopping);
        allowed
    }

    fn interactive_input(&self) -> bool {
        true
    }

    fn ask_questions(
        &mut self,
        request: &agent::workflow::questions::QuestionRequest,
        cancel: &AtomicBool,
    ) -> agent::workflow::questions::QuestionOutcome {
        use crate::app::shell::prompts::{DraftStep, Focus};
        use agent::workflow::questions::QuestionOutcome;
        let mut dialog = crate::ui::questions::Dialog::new(request, &self.app.layout.project_root)
            .with_keys(&self.shell.keys);
        if request.notify && self.shell.ui.notify {
            crate::ui::termcaps::notify(
                crate::ui::termcaps::NotifyKind::Question,
                "Dial",
                "A response is needed",
                &self.shell.session_id,
            );
        }
        let was_stopping = self.interrupt.load(Ordering::SeqCst);
        if self.shell.park_pending_input() {
            // The stop key arrived before the dialog: stop, never answer.
            self.show_stopping(None);
            return QuestionOutcome::Cancelled;
        }
        let was_waiting = std::mem::replace(&mut self.shell.awaiting_user, true);
        let mut focus = self.shell.initial_focus();
        let mut guard = TypeAhead::default();
        let outcome = loop {
            if cancel.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst) {
                break QuestionOutcome::Cancelled;
            }
            let (width, height) = self.shell.term.size();
            let draft_note = self.shell.draft_note(focus);
            let draft_height = if draft_note.is_some() { 4 } else { 0 };
            let inner_width = usize::from(width).saturating_sub(4);
            let mut body = dialog.rows(
                inner_width,
                usize::from(height).saturating_sub(6 + draft_height),
            );
            body.insert(1.min(body.len()), ATTENTION_HINT.into());
            if let Some(note) = draft_note {
                body.extend(note.lines().take(draft_height).map(|line| {
                    crate::ui::text::ellipsize(
                        &crate::ui::text::sanitize(line),
                        inner_width,
                        self.shell.themes.current().ascii(),
                    )
                }));
            }
            let rows = crate::ui::questions::framed(
                body,
                usize::from(width),
                usize::from(height).saturating_sub(1),
                self.shell.themes.current(),
            );
            if self.shell.paint(Some(rows)).is_err() {
                break QuestionOutcome::Disconnected;
            }
            guard.painted(Instant::now());
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
                    if attention_key(&key) {
                        dialog.pause_timeout(Instant::now());
                        let pending = dialog
                            .rows(inner_width, usize::from(height).saturating_sub(5))
                            .join("\n");
                        let exit =
                            self.shell
                                .inspect_modal_attention("Main question", &pending, || {
                                    cancel.load(Ordering::SeqCst)
                                        || self.interrupt.load(Ordering::SeqCst)
                                });
                        match exit {
                            AttentionExit::Cancelled => break QuestionOutcome::Cancelled,
                            AttentionExit::Disconnected => break QuestionOutcome::Disconnected,
                            AttentionExit::Returned => {
                                if let Err(error) = dialog.resume_timeout(Instant::now()) {
                                    break QuestionOutcome::Unsupported(error);
                                }
                                continue;
                            }
                        }
                    }
                    if focus == Focus::Draft {
                        match self.shell.draft_step(key) {
                            DraftStep::Stay => {}
                            DraftStep::Choices => focus = Focus::Choices,
                            DraftStep::Cancel => {
                                self.interrupt.store(true, Ordering::SeqCst);
                                break QuestionOutcome::Cancelled;
                            }
                        }
                        continue;
                    }
                    if stop_key(&self.shell.keys, &key) {
                        self.interrupt.store(true, Ordering::SeqCst);
                        break QuestionOutcome::Cancelled;
                    }
                    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                        focus = Focus::Draft;
                        continue;
                    }
                    if guard.blocks(&key, Instant::now()) {
                        continue;
                    }
                    if let Some(outcome) = dialog.key(key, &self.shell.keys) {
                        break outcome;
                    }
                }
                Ok(Event::Paste(value)) => match focus {
                    Focus::Draft => self.shell.composer_paste(&value),
                    Focus::Choices => dialog.paste(&value),
                },
                Ok(Event::Resize(..)) => self.shell.term.on_resize(),
                Ok(_) => {}
                Err(_) => break QuestionOutcome::Disconnected,
            }
        };
        self.shell.awaiting_user = was_waiting;
        self.settle_prompt(was_stopping);
        outcome
    }

    fn ask_user(&mut self, title: &str, question: &str, options: &[String]) -> Option<String> {
        let mut choices: Vec<(String, char)> = options
            .iter()
            .enumerate()
            .map(|(index, option)| (option.clone(), ask_user_hotkey(index)))
            .collect();
        choices.push(("Other (type your own)".into(), 'o'));
        let choice_refs: Vec<(&str, char)> = choices
            .iter()
            .map(|(label, key)| (label.as_str(), *key))
            .collect();
        let interrupt = self.interrupt.clone();
        let was_stopping = interrupt.load(Ordering::SeqCst);
        let was_waiting = std::mem::replace(&mut self.shell.awaiting_user, true);
        let answer =
            match self
                .shell
                .ask_with_cancel(title, question, &choice_refs, Some(&interrupt))
            {
                Some(index) if index < options.len() => Some(options[index].clone()),
                Some(_) => self.line_prompt(
                    &format!("{} (Enter sends, Esc cancels)", text::one_line(question)),
                    title,
                    question,
                ),
                None => None,
            };
        self.shell.awaiting_user = was_waiting;
        self.settle_prompt(was_stopping);
        answer
    }

    fn approval_reason(&mut self) -> Option<String> {
        let was_stopping = self.interrupt.load(Ordering::SeqCst);
        let was_waiting = std::mem::replace(&mut self.shell.awaiting_user, true);
        let reason = self.line_prompt(
            "Keep planning: what should change? (Enter sends, Esc skips)",
            "Plan feedback pending",
            "Return to the feedback field before entering a reason. No plan can be approved here.",
        );
        self.shell.awaiting_user = was_waiting;
        self.settle_prompt(was_stopping);
        reason
    }
}

#[cfg(test)]
mod request_settings_tests;

#[cfg(test)]
mod approval_tests;

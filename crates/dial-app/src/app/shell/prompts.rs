//! Asking the user a question from inside a turn, with optional cancellation.
//!
//! A question has two focus states. Choices own the keyboard: letters are hotkeys and Enter
//! answers. The composer draft owns the keyboard while it has focus, so typing never approves
//! anything. A non-empty draft starts with focus, and Tab hands focus between the two.

use crate::app::panel::{
    PanelView, RECOVERY_PANEL, panel_body_limit, panel_has_room, panel_inner, panel_rows_window,
    panel_scroll,
};
use crate::app::shell::{Shell, TICK};
use crate::ui::composer::ComposerEvent;
use crate::ui::input::{poll, read};
use crate::ui::keys::Action;
use crate::ui::text;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Where keystrokes go while a question is open.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Focus {
    Choices,
    Draft,
}

/// What one key did while the draft had focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum DraftStep {
    Stay,
    Choices,
    Cancel,
}

pub(in crate::app) const ATTENTION_HINT: &str =
    "F6: read-only Attention (keeps this prompt pending)";

pub(in crate::app) fn attention_key(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release && key.code == KeyCode::F(6) && key.modifiers.is_empty()
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::app) enum AttentionExit {
    Returned,
    Cancelled,
    Disconnected,
}

/// This reducer has no command, answer or draft channel. All inspector input is consumed.
#[derive(Default)]
struct AttentionView {
    offset: usize,
}

impl AttentionView {
    fn event(
        &mut self,
        event: &Event,
        keys: &crate::ui::keys::Keymap,
        lines: usize,
        visible: usize,
    ) -> Option<AttentionExit> {
        let Event::Key(key) = event else {
            return None;
        };
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if attention_key(key) || key.code == KeyCode::Esc {
            return Some(AttentionExit::Returned);
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(AttentionExit::Cancelled);
        }
        let down = keys.matches(Action::SelectPageDown, key);
        if down || keys.matches(Action::SelectPageUp, key) {
            self.offset = panel_scroll(self.offset, lines, visible, down);
        }
        None
    }
}

impl Shell {
    // ----- asking the user ------------------------------------------------------------------

    /// Frozen observed data, not another Activity controller. Action values are deliberately
    /// discarded; this view can only return to its still-owned prompt or cancel it.
    fn modal_attention_text(&self, title: &str, body: &str) -> String {
        let workers = crate::ui::subagent_model::HubSnapshot {
            agents: self.workers.agents().to_vec(),
            ..Default::default()
        };
        let jobs = self
            .background_control
            .as_ref()
            .and_then(|control| control.jobs_view(None))
            .map(|view| {
                view.jobs
                    .into_iter()
                    .map(|job| crate::ui::overlay::jobs::JobRow {
                        name: job.name,
                        command: job.command,
                        status: job.status,
                        process_outcome: job.process_outcome,
                        running: job.running,
                        terminal: job.terminal,
                        mode: "session".into(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let failure = self
            .model_downloads
            .as_ref()
            .and_then(|downloads| downloads.borrow().last_failure().cloned());
        let projection = crate::app::overlay::activity::attention_choices_with_context(
            &workers,
            &jobs,
            Some(&self.status),
            Some(&self.todo),
            failure.as_ref(),
            self.last_setup_failure.as_ref(),
        );
        let mut text = format!(
            "F6 / Esc: Return to original prompt · Ctrl+C: cancel original prompt\nRead-only frozen snapshot. Enter, approval letters and paste do nothing.\n\nMain prompt · waiting for you\n{title}\n{body}"
        );
        for item in projection.items {
            text.push_str(&format!("\n\n{}\n{}", item.label, item.detail));
        }
        text.push_str("\n\nWorker data is last observed, not refreshed here. Missing snapshots are not an all-clear. Return to the original prompt before operating any owner.");
        text
    }

    pub(in crate::app) fn inspect_modal_attention(
        &mut self,
        title: &str,
        body: &str,
        cancelled: impl Fn() -> bool,
    ) -> AttentionExit {
        let body = self.modal_attention_text(title, body);
        let mut view = AttentionView::default();
        loop {
            if cancelled() {
                return AttentionExit::Cancelled;
            }
            let (cols, height) = self.term.size();
            let available = height.saturating_sub(6) as usize;
            let rows = panel_rows_window(
                self.themes.current(),
                cols,
                available,
                PanelView {
                    title: "Attention · read-only · F6/Esc returns to prompt",
                    body: &body,
                    options: &[],
                    selected: 0,
                    offset: view.offset,
                },
            );
            if self.paint(Some(rows)).is_err() {
                return AttentionExit::Disconnected;
            }
            match poll(TICK) {
                Ok(false) => continue,
                Err(_) => return AttentionExit::Disconnected,
                Ok(true) => {}
            }
            let event = match read() {
                Ok(event) => event,
                Err(_) => return AttentionExit::Disconnected,
            };
            if cancelled() {
                return AttentionExit::Cancelled;
            }
            if matches!(event, Event::Resize(..)) {
                self.term.on_resize();
            }
            let lines = text::wrap_words(&text::sanitize(&body), panel_inner(cols)).len();
            let visible = panel_body_limit(0, available);
            if let Some(exit) = view.event(&event, &self.keys, lines, visible) {
                return exit;
            }
        }
    }

    /// Keep draft-only consent across bridges and turns, but not across app launches.
    pub(in crate::app) fn review_turn_draft(
        &mut self,
        cancel: &std::sync::atomic::AtomicBool,
        ask: impl FnOnce(&mut Self) -> Option<usize>,
    ) -> bool {
        use std::sync::atomic::Ordering;

        if cancel.load(Ordering::Acquire) {
            return false;
        }
        if self.accept_all_turn_drafts {
            return true;
        }
        let choice = ask(self);
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        match choice {
            Some(0) => true,
            Some(1) => {
                self.accept_all_turn_drafts = true;
                true
            }
            _ => false,
        }
    }

    /// Keys typed before a question appeared are parked for the main loop, so they cannot answer
    /// the question. The composer keeps any draft text it already holds.
    pub(in crate::app) fn park_pending_input(&mut self) {
        while matches!(poll(std::time::Duration::ZERO), Ok(true)) {
            match read() {
                Ok(Event::Paste(text)) => self.composer_paste(&text),
                Ok(Event::Key(key))
                    if key.kind != KeyEventKind::Release
                        && (key.modifiers - KeyModifiers::SHIFT).is_empty()
                        && matches!(
                            key.code,
                            KeyCode::Char(_)
                                | KeyCode::Backspace
                                | KeyCode::Delete
                                | KeyCode::Left
                                | KeyCode::Right
                                | KeyCode::Home
                                | KeyCode::End
                        ) =>
                {
                    self.draft_key(key);
                }
                Ok(event) => self.held.push_back(event),
                Err(_) => break,
            }
        }
    }

    /// A modal choice drawn in place of the composer. `None` means the user cancelled.
    pub(in crate::app) fn ask(
        &mut self,
        title: &str,
        body: &str,
        options: &[(&str, char)],
    ) -> Option<usize> {
        self.ask_with_cancel(title, body, options, None)
    }

    pub(in crate::app) fn ask_with_cancel(
        &mut self,
        title: &str,
        body: &str,
        options: &[(&str, char)],
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<usize> {
        if options.is_empty() {
            return None;
        }
        self.park_pending_input();
        // The terminal tab shows that Dial waits for an answer while the panel is up.
        let was_waiting = std::mem::replace(&mut self.awaiting_user, true);
        let answer = self.ask_loop(title, body, options, cancel);
        self.awaiting_user = was_waiting;
        answer
    }

    fn ask_loop(
        &mut self,
        title: &str,
        body: &str,
        options: &[(&str, char)],
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<usize> {
        let mut body_offset = 0usize;
        let mut selected = 0usize;
        let mut show_raw = false;
        let mut focus = self.initial_focus();
        loop {
            if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire)) {
                return None;
            }
            let mut shown = if title == RECOVERY_PANEL {
                format!(
                    "{}\n\n{} {}",
                    crate::ui::recovery::panel_text(body, show_raw),
                    self.keys.label(Action::ToolsExpand),
                    if show_raw {
                        "to hide details"
                    } else {
                        "for details"
                    }
                )
            } else {
                body.to_owned()
            };
            shown = format!("{ATTENTION_HINT}\n\n{shown}");
            let draft = self.composer.draft();
            if let Some(note) = self.draft_note(focus) {
                shown = format!("{shown}\n\n{note}");
            }
            let (cols, height) = self.term.size();
            let rows = panel_rows_window(
                self.themes.current(),
                cols,
                height.saturating_sub(6) as usize,
                PanelView {
                    title,
                    body: &shown,
                    options,
                    selected,
                    offset: body_offset,
                },
            );
            if self.paint(Some(rows)).is_err() {
                return None;
            }
            if !poll(TICK).ok()? {
                continue;
            }
            match read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                    if attention_key(&key) {
                        match self.inspect_modal_attention(title, &shown, || {
                            cancel
                                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
                        }) {
                            AttentionExit::Returned => continue,
                            AttentionExit::Cancelled | AttentionExit::Disconnected => return None,
                        }
                    }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if !panel_has_room(cols, height.saturating_sub(6) as usize) {
                        // A tiny viewport cannot show what a confirmation would authorize. Keep
                        // cancellation available, but never act on Enter or an approval hotkey.
                        if self.keys.matches(Action::Dismiss, &key)
                            || key.code == KeyCode::Esc
                            || (key.code == KeyCode::Char('c') && ctrl)
                        {
                            return None;
                        }
                        continue;
                    }
                    // Page by the body rows actually visible, keeping one row of overlap, so a short
                    // terminal never skips details the user has not seen. Paging works in both focuses.
                    let down = self.keys.matches(Action::SelectPageDown, &key);
                    if down || self.keys.matches(Action::SelectPageUp, &key) {
                        let (cols, height) = self.term.size();
                        let lines =
                            text::wrap_words(&text::sanitize(&shown), panel_inner(cols)).len();
                        let visible =
                            panel_body_limit(options.len(), height.saturating_sub(6) as usize);
                        body_offset = panel_scroll(body_offset, lines, visible, down);
                        continue;
                    }
                    if focus == Focus::Draft {
                        match self.draft_step(key) {
                            DraftStep::Stay => {}
                            DraftStep::Choices => focus = Focus::Choices,
                            DraftStep::Cancel => return None,
                        }
                        continue;
                    }
                    if title == RECOVERY_PANEL && self.keys.matches(Action::ToolsExpand, &key) {
                        show_raw = !show_raw;
                        body_offset = 0;
                        continue;
                    }
                    if self.keys.matches(Action::Dismiss, &key) {
                        return None;
                    }
                    if key.code == KeyCode::Tab && !draft.is_empty() {
                        focus = Focus::Draft;
                        continue;
                    }
                    if self.keys.matches(Action::SelectUp, &key) {
                        selected = (selected + options.len() - 1) % options.len();
                        continue;
                    }
                    if self.keys.matches(Action::SelectDown, &key) {
                        selected = (selected + 1) % options.len();
                        continue;
                    }
                    match key.code {
                        KeyCode::Up => selected = (selected + options.len() - 1) % options.len(),
                        KeyCode::Down | KeyCode::Tab => {
                            selected = (selected + 1) % options.len();
                        }
                        KeyCode::Enter => return Some(selected),
                        KeyCode::Esc => return None,
                        KeyCode::Char('c') if ctrl => return None,
                        KeyCode::Char(c) if !ctrl => {
                            if let Some(index) = options.iter().position(|(_, hotkey)| {
                                *hotkey != ' ' && hotkey.eq_ignore_ascii_case(&c)
                            }) {
                                return Some(index);
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Resize(..)) => self.term.on_resize(),
                Ok(Event::Paste(text)) if focus == Focus::Draft => self.composer_paste(&text),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    /// Where a prompt's keys go first. A draft typed before the prompt keeps the keyboard, so its
    /// next keys cannot answer the prompt.
    pub(in crate::app) fn initial_focus(&self) -> Focus {
        if self.composer.draft().is_empty() {
            Focus::Choices
        } else {
            Focus::Draft
        }
    }

    /// The draft as shown under a prompt, with the key that moves focus. `None` when empty.
    pub(in crate::app) fn draft_note(&self, focus: Focus) -> Option<String> {
        let draft = self.composer.draft();
        if draft.is_empty() && focus == Focus::Choices {
            return None;
        }
        let hint = match focus {
            Focus::Choices => "Tab to edit your draft",
            Focus::Draft => "Tab to return to the choices",
        };
        Some(format!("Your draft ({hint}):\n{draft}"))
    }

    /// Routes one key pressed while the draft has focus. A newline never submits.
    pub(in crate::app) fn draft_step(&mut self, key: KeyEvent) -> DraftStep {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Tab => DraftStep::Choices,
            KeyCode::Char('c') if ctrl => DraftStep::Cancel,
            _ if self.keys.matches(Action::Dismiss, &key) => DraftStep::Choices,
            KeyCode::Enter => {
                self.composer.handle_paste("\n");
                DraftStep::Stay
            }
            _ => {
                self.draft_key(key);
                DraftStep::Stay
            }
        }
    }

    /// Edits the draft during a prompt. Submitting or running a composer action would act on the
    /// prompt's behalf, so the composer only keeps the text it would have submitted.
    fn draft_key(&mut self, key: KeyEvent) {
        if let ComposerEvent::Submit(text) = self.composer_key(key) {
            self.composer.set_draft(&text);
        }
    }
}

#[cfg(test)]
mod tests;

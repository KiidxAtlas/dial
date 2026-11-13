//! Busy-turn routing: queued messages, explicit steers and the busy chooser.
use super::super::{
    keys::{Action, Keymap},
    theme::{Sym, Theme, Tok},
};
use super::paste::expand_pastes;
use super::{
    BusyChoice, Choice, Composer, ComposerCtx, ComposerEvent, Explicit, QueuedEdit, SteerKind,
    SteerStage,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{style::Modifier, text::Span};

/// How long a recorded steer or started worker stays in the strip: long enough to read that it
/// landed, short enough that the strip only ever shows what is still in flight.
pub(super) const STEER_FADE: std::time::Duration = std::time::Duration::from_secs(4);

impl Explicit {
    pub(super) fn new(text: String, kind: SteerKind) -> Self {
        Explicit {
            text,
            kind,
            stage: SteerStage::Pending,
            untaken: true,
            applied_at: None,
        }
    }

    /// Still shown: not yet applied, or applied within the fade window.
    pub(super) fn visible(&self, now: std::time::Instant) -> bool {
        self.applied_at
            .is_none_or(|at| now.saturating_duration_since(at) < STEER_FADE)
    }
}

/// The stage trail that leads a steer or worker row, one mark per stage (pending, received,
/// recorded): reached stages in the row's color, the rest dim, a check once it is recorded. The
/// one row changes in place as the agent reports each stage: ` ●─●─○ Steer`.
pub(super) fn steer_trail(theme: &Theme, stage: SteerStage, tok: Tok) -> Vec<Span<'static>> {
    let (reached, open, link) = if theme.ascii() {
        ("*", ".", "-")
    } else {
        ("●", "○", "─")
    };
    let mut spans = vec![Span::raw(" ")];
    for (i, step) in [
        SteerStage::Pending,
        SteerStage::Received,
        SteerStage::Applied,
    ]
    .into_iter()
    .enumerate()
    {
        if i > 0 {
            spans.push(Span::styled(link, theme.fg(Tok::Dim)));
        }
        let (glyph, style) = if step == SteerStage::Applied && stage == SteerStage::Applied {
            (
                theme.sym(Sym::Check),
                theme.fg(tok).add_modifier(Modifier::BOLD),
            )
        } else if step <= stage {
            (reached, theme.fg(tok).add_modifier(Modifier::BOLD))
        } else {
            (open, theme.fg(Tok::Dim))
        };
        spans.push(Span::styled(glyph.to_owned(), style));
    }
    spans
}

impl BusyChoice {
    pub(super) fn label(self) -> &'static str {
        match self {
            BusyChoice::Auto => "Let Dial choose (auto)",
            BusyChoice::Redirect => "Redirect current task now",
            BusyChoice::AfterTurn => "After this turn",
            BusyChoice::Independent => "Run independently",
            BusyChoice::KeepWorking => "Keep working",
            BusyChoice::StopAndSend => "Stop and send oldest",
        }
    }

    /// A few words for the busy hint: `redirect`, `after turn`.
    pub(super) fn short(self) -> &'static str {
        match self {
            BusyChoice::Auto => "auto",
            BusyChoice::Redirect => "redirect",
            BusyChoice::AfterTurn => "after turn",
            BusyChoice::Independent => "independent",
            BusyChoice::KeepWorking => "keep working",
            BusyChoice::StopAndSend => "stop and send",
        }
    }

    pub(super) fn detail(self) -> &'static str {
        match self {
            BusyChoice::Auto => "uses configured routing; may redirect or request a worker",
            BusyChoice::Redirect => {
                "changes this task after the current step; running tools finish"
            }
            BusyChoice::AfterTurn => "queues this message; leaves the current task unchanged",
            BusyChoice::Independent => {
                "requests a separate worker; otherwise queues after this turn"
            }
            BusyChoice::KeepWorking => "the queue waits for this turn",
            BusyChoice::StopAndSend => "ends this turn now",
        }
    }
}

/// How a queued message will be sent, as shown in the queue preview and while editing it.
pub(super) fn queued_route(follow_up: bool) -> &'static str {
    if follow_up {
        "send after turn"
    } else {
        "send now"
    }
}

/// Offered for a non-empty draft while busy. Automatic routing is the default.
pub(super) const DRAFT_CHOICES: &[BusyChoice] = &[
    BusyChoice::Auto,
    BusyChoice::Redirect,
    BusyChoice::AfterTurn,
    BusyChoice::Independent,
];

/// Offered for an empty Enter with queued work. Stopping the turn is never the default.
pub(super) const QUEUE_CHOICES: &[BusyChoice] = &[BusyChoice::KeepWorking, BusyChoice::StopAndSend];

impl Composer {
    pub fn take_steering(&mut self) -> Vec<String> {
        if self.recovery_held
            || !self.queued.iter().any(|(_, follow_up)| !follow_up)
            || !self.recovery_before_take(super::recovery::Take::Steering)
        {
            return Vec::new();
        }
        let mut steering = Vec::new();
        for _ in 0..self.queued.len() {
            if let Some((text, follow_up)) = self.queued.pop_front() {
                if follow_up {
                    self.queued.push_back((text, true));
                } else {
                    steering.push(text);
                }
            }
        }
        steering
    }

    /// Explicit "Steer now" messages not yet handed to the agent, oldest first.
    pub fn take_explicit_steering(&mut self) -> Vec<String> {
        self.take_explicit(SteerKind::Steer)
    }

    /// Explicit "Independent worker" messages not yet handed to the agent, oldest first.
    pub fn take_explicit_independent(&mut self) -> Vec<String> {
        self.take_explicit(SteerKind::Independent)
    }

    pub(super) fn take_explicit(&mut self, kind: SteerKind) -> Vec<String> {
        if self.recovery_held
            || !self
                .explicit
                .iter()
                .any(|item| item.kind == kind && item.untaken)
            || !self.recovery_before_take(super::recovery::Take::Explicit(kind))
        {
            return Vec::new();
        }
        self.explicit
            .iter_mut()
            .filter(|item| item.kind == kind && item.untaken)
            .map(|item| {
                item.untaken = false;
                item.text.clone()
            })
            .collect()
    }

    /// Advance the oldest matching explicit message to `stage`. Unknown text (automatically routed
    /// input) is ignored, and a stage never moves backwards.
    pub fn mark_explicit(&mut self, text: &str, stage: SteerStage) -> bool {
        let found = if let Some(item) = self
            .explicit
            .iter_mut()
            .find(|item| item.text == text && item.stage < stage)
        {
            item.stage = stage;
            item.untaken = false;
            if stage == SteerStage::Applied {
                item.applied_at = Some(std::time::Instant::now());
            }
            true
        } else {
            false
        };
        // Keep the strip short: only the newest finished ones stay visible.
        while self
            .explicit
            .iter()
            .filter(|item| item.stage == SteerStage::Applied)
            .count()
            > 2
        {
            if let Some(index) = self
                .explicit
                .iter()
                .position(|item| item.stage == SteerStage::Applied)
            {
                self.explicit.remove(index);
            }
        }
        found
    }

    /// Put messages the agent could not start as separate work back as follow-ups for after the
    /// turn; they are never turned into steering of the running task.
    pub fn defer_messages(&mut self, messages: Vec<String>) {
        for message in messages.into_iter().rev() {
            // Deferral transfers ownership to the follow-up queue. It was not applied, but
            // must no longer also be recovered from the explicit strip at turn end.
            if let Some(index) = self.explicit.iter().position(|item| {
                item.kind == SteerKind::Independent
                    && !item.untaken
                    && item.stage != SteerStage::Applied
                    && item.text == message
            }) {
                self.explicit.remove(index);
            }
            self.queued.push_front((message, true));
        }
    }

    /// Admit a slash-command worker request without replacing an unrelated editor draft.
    pub fn request_worker(&mut self, text: &str) -> bool {
        if text.trim().is_empty() || text.len() > 256 * 1024 || self.explicit.len() >= 16 {
            return false;
        }
        self.explicit
            .push(Explicit::new(text.to_owned(), SteerKind::Independent));
        true
    }

    pub(super) fn send_explicit(&mut self, kind: SteerKind) -> ComposerEvent {
        if !self.attachment_send_allowed() {
            return ComposerEvent::None;
        }
        if self.editor.text.trim().is_empty() {
            return ComposerEvent::None;
        }
        let sent = expand_pastes(&self.editor.text, &self.pastes);
        if sent.len() > 256 * 1024 || self.explicit.len() >= 16 {
            self.queue_notice =
                Some("Too many unsent steering messages. Draft kept; wait for Dial.".into());
            return ComposerEvent::None;
        }
        self.learn_words(&sent);
        self.push_history(sent.clone());
        // An edited queue item sent explicitly leaves the queue: the explicit route replaces it.
        self.editing_queue = None;
        self.explicit.push(Explicit::new(sent, kind));
        self.finish_draft();
        self.queue_notice = None;
        ComposerEvent::None
    }

    /// Return unhandled automatic input in submission order; it already passed queue admission.
    pub fn restore_pending_messages(&mut self, messages: Vec<String>) {
        for message in messages.into_iter().rev() {
            self.queued.push_front((message, false));
        }
    }

    pub fn take_follow_up(&mut self) -> Option<String> {
        if self.recovery_held
            || self.queued.is_empty()
            || !self.recovery_before_take(super::recovery::Take::FollowUp)
        {
            return None;
        }
        self.queued.pop_front().map(|(text, _)| text)
    }

    pub fn has_queued_work(&self) -> bool {
        !self.recovery_held && !self.queued.is_empty()
    }

    /// Busy input the agent will take at its next step boundary: routable queued messages and
    /// untaken explicit messages. Follow-ups deferred until after the turn do not count.
    pub fn has_pending_input(&self) -> bool {
        if self.recovery_held {
            return false;
        }
        self.queued.iter().any(|(_, follow_up)| !follow_up)
            || self.explicit.iter().any(|item| item.untaken)
    }

    pub fn queued_messages(&self) -> Vec<(String, bool)> {
        self.queued.iter().cloned().collect()
    }

    pub fn remove_queued(&mut self, index: usize) -> bool {
        let removed = self.queued.remove(index).is_some();
        if removed {
            self.queue_notice = None;
        }
        removed
    }

    pub fn edit_queued(&mut self, index: usize) -> bool {
        if !self.editor.text.is_empty() {
            return false;
        }
        self.restore_queued_edit();
        let Some((text, follow_up)) = self.queued.remove(index) else {
            return false;
        };
        self.begin_queued_edit(index, text, follow_up);
        true
    }

    /// Load a queued item (already removed from the queue) into the draft, remembering where it came from.
    pub(super) fn begin_queued_edit(&mut self, index: usize, text: String, follow_up: bool) {
        self.set_draft(&text);
        self.editing_queue = Some(QueuedEdit {
            index,
            text,
            follow_up,
        });
    }

    pub(super) fn restore_queued_edit(&mut self) {
        if let Some(edit) = self.editing_queue.take() {
            let index = edit.index.min(self.queued.len());
            self.queued.insert(index, (edit.text, edit.follow_up));
        }
    }

    /// Up/Down step through queued messages before history: Up on an empty draft edits the newest
    /// queued message, Up again the one before it, and past the oldest it continues into history.
    /// Down walks back toward the newest and then to an empty draft. Only an unchanged (or cleared)
    /// queue edit steps, so typed changes are never discarded by an arrow key.
    pub(super) fn step_queue(&mut self, key: &KeyEvent, keys: &Keymap) -> bool {
        let up = keys.matches(Action::SelectUp, key);
        if !(up || keys.matches(Action::SelectDown, key)) || self.menu_open() {
            return false;
        }
        let text = &self.editor.text;
        let at_edge = if up {
            !text[..self.editor.cursor].contains('\n')
        } else {
            !text[self.editor.cursor..].contains('\n')
        };
        if let Some(edit) = &self.editing_queue
            && self.history_index.is_none()
        {
            if !at_edge || !(text.is_empty() || *text == edit.text) {
                return false;
            }
            let index = edit.index;
            self.set_draft("");
            let next = if up {
                index.checked_sub(1)
            } else {
                Some(index + 1).filter(|next| *next < self.queued.len())
            };
            match next {
                Some(next) => {
                    if let Some((text, follow_up)) = self.queued.remove(next) {
                        self.begin_queued_edit(next, text, follow_up);
                    }
                }
                // Past the oldest queued message: on into prompt history.
                None if up => self.browse_history(true),
                None => {}
            }
            return true;
        }
        if up && text.is_empty() && self.history_index.is_none() && !self.queued.is_empty() {
            let index = self.queued.len() - 1;
            if let Some((text, follow_up)) = self.queued.remove(index) {
                self.begin_queued_edit(index, text, follow_up);
            }
            return true;
        }
        false
    }

    pub(super) fn submit_queued(&mut self, follow_up: bool) -> ComposerEvent {
        if !self.attachment_send_allowed() {
            return ComposerEvent::None;
        }
        if self.editor.text.trim().is_empty() {
            return ComposerEvent::None;
        }
        let sent = expand_pastes(&self.editor.text, &self.pastes);
        let bytes: usize = self.queued.iter().map(|(text, _)| text.len()).sum();
        if self.queued.len() >= 32
            || sent.len() > 256 * 1024
            || bytes.saturating_add(sent.len()) > 1024 * 1024
        {
            self.queue_notice = Some(
                "Queue limit reached. Draft kept; use /queue to edit or remove messages.".into(),
            );
            return ComposerEvent::None;
        }
        self.learn_words(&sent);
        self.push_history(sent.clone());
        // A saved edit replaces the original in place, so editing never reorders the queue.
        let edit = self.editing_queue.take();
        // A saved edit keeps the route it was queued with; the caller's choice only applies to new messages.
        let follow_up = edit.as_ref().map_or(follow_up, |edit| edit.follow_up);
        let index = edit.map_or(self.queued.len(), |edit| edit.index.min(self.queued.len()));
        self.queued.insert(index, (sent, follow_up));
        self.finish_draft();
        self.queue_notice = None;
        ComposerEvent::None
    }

    /// Opens the busy chooser. The draft chooser highlights the route picked last time this session
    /// (else automatic routing); the queue chooser always opens on its safe default, since stopping
    /// the turn is never preselected. The draft is left as it is.
    pub(super) fn open_choice(&mut self, options: &'static [BusyChoice]) {
        let remembered = (options == DRAFT_CHOICES)
            .then_some(self.last_draft_choice)
            .flatten()
            .and_then(|last| options.iter().position(|option| *option == last));
        self.choice = Some(Choice {
            options,
            selected: remembered.unwrap_or(0),
        });
        self.exit_armed = false;
    }

    /// Keys while the busy chooser is open. `Some` is the event for a key it consumed. `None` closes
    /// the chooser and lets the key reach ordinary editing, so typed or pasted text is never dropped.
    pub(super) fn choice_key(&mut self, key: KeyEvent, ctx: &ComposerCtx) -> Option<ComposerEvent> {
        let keys = ctx.keys;
        let (options, selected) = {
            let choice = self.choice.as_ref()?;
            (choice.options, choice.selected)
        };
        if keys.matches(Action::Dismiss, &key) {
            self.choice = None;
            return Some(ComposerEvent::None);
        }
        // Digits pick an option directly; Ctrl+Enter (kitty keyboard protocol only) redirects.
        // Letters and editing chords never act here: they close the chooser and reach the draft.
        let direct = match key.code {
            KeyCode::Char(c @ '1'..='9') if key.modifiers.is_empty() => {
                Some(c as usize - '1' as usize).filter(|index| *index < options.len())
            }
            KeyCode::Enter
                if key.modifiers == KeyModifiers::CONTROL && options == DRAFT_CHOICES =>
            {
                options
                    .iter()
                    .position(|option| *option == BusyChoice::Redirect)
            }
            _ => None,
        };
        if direct.is_none()
            && (keys.matches(Action::SelectUp, &key) || keys.matches(Action::SelectDown, &key))
        {
            let next = if keys.matches(Action::SelectUp, &key) {
                selected.saturating_sub(1)
            } else {
                (selected + 1).min(options.len() - 1)
            };
            if let Some(choice) = self.choice.as_mut() {
                choice.selected = next;
            }
            return Some(ComposerEvent::None);
        }
        if direct.is_some() || keys.matches(Action::Submit, &key) {
            self.choice = None;
            let picked = options[direct.unwrap_or(selected)];
            if options == DRAFT_CHOICES {
                self.last_draft_choice = Some(picked);
            }
            let event = match picked {
                BusyChoice::Auto => self.submit_queued(false),
                BusyChoice::Redirect => self.send_explicit(SteerKind::Steer),
                BusyChoice::Independent => self.send_explicit(SteerKind::Independent),
                BusyChoice::AfterTurn => self.submit_queued(true),
                BusyChoice::KeepWorking => ComposerEvent::None,
                BusyChoice::StopAndSend => self.send_oldest_queued(),
            };
            return Some(event);
        }
        if self.busy && keys.matches(Action::Interrupt, &key) {
            self.choice = None;
            return Some(ComposerEvent::Interrupt);
        }
        self.choice = None;
        None
    }

    /// Sends the oldest queued message now. Only an explicit stop does this; queuing already recorded it.
    pub(super) fn send_oldest_queued(&mut self) -> ComposerEvent {
        if !self.recovery_before_take(super::recovery::Take::FollowUp) {
            return ComposerEvent::None;
        }
        let Some((text, _)) = self.queued.pop_front() else {
            return ComposerEvent::None;
        };
        self.queue_notice = None;
        ComposerEvent::Submit(text)
    }
}

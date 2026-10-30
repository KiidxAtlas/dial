//! Voice input and spoken replies, driven by the push-to-talk key.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::shell::Shell;
use crate::config::voice::{InputMode, SubmitTrigger};
use crate::ui::composer::ComposerEvent;
use crate::ui::keys::Action;
use crossterm::event::KeyEventKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

impl Shell {
    /// Start listening or finish the current recording. A tap starts silence-ended capture by
    /// default; hold-to-talk is an explicit setting, not an assumption about the user's gesture.
    pub(in crate::app) fn voice_press(&mut self, app: &App) {
        // Once stopped, keep the worker until it delivers its transcript; another press must not
        // replace it and silently lose an in-flight transcription.
        if self.voice_input.is_some() && !self.voice_gesture.recording() {
            return;
        }
        let hold = app.config.voice.stt.input_mode == InputMode::Hold
            && crate::ui::term::reports_key_releases();
        match self.voice_gesture.press(hold) {
            crate::voice::gesture::Command::Stop => self.stop_voice(),
            crate::voice::gesture::Command::Start { hold } => {
                if !app.config.voice.stt.enabled {
                    self.voice_gesture.finished();
                    self.note(
                        "Voice",
                        "Dictation is off. Turn on Dictation under Voice in /settings.",
                    );
                    return;
                }
                self.stop_speech();
                match crate::voice::tui::PushToTalk::start(
                    crate::voice::VoiceEnv::from_config(&app.config),
                    hold,
                ) {
                    Ok(capture) => {
                        self.voice_input = Some(capture);
                        self.composer.set_recording(true, hold);
                    }
                    Err(error) => {
                        self.voice_gesture.finished();
                        self.note("Voice", format!("{error:#}"));
                    }
                }
            }
        }
    }

    /// Voice key release: ends a hold-started recording. Releases of toggle recordings are ignored.
    fn voice_release(&mut self) {
        if self.voice_gesture.release().is_some() {
            self.stop_voice();
        }
    }

    fn stop_voice(&self) {
        if let Some(capture) = &self.voice_input {
            capture.stop();
        }
    }

    /// Releases and auto-repeats of the voice key never reach normal key handling: a release ends a
    /// held recording, and a repeat must not toggle it again. Returns true when the key was consumed.
    pub(in crate::app) fn consume_voice_edge(&mut self, key: &crossterm::event::KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release && self.keys.release_matches(Action::VoiceInput, key) {
            self.voice_release();
            return true;
        }
        key.kind == KeyEventKind::Repeat && self.keys.matches(Action::VoiceInput, key)
    }

    /// Collects a finished capture or read-out. Returns true when the screen needs a repaint.
    pub(in crate::app) fn poll_voice(&mut self, app: &App) -> bool {
        let mut changed = false;
        if let Some(result) = self
            .voice_input
            .as_ref()
            .and_then(|capture| capture.try_result())
        {
            self.voice_input = None;
            self.voice_gesture.finished();
            self.composer.set_recording(false, false);
            changed = true;
            match result {
                Ok(text) if !text.trim().is_empty() => self.voice_transcript(app, text.trim()),
                Ok(_) => self.note("Voice", "No speech was heard."),
                Err(error) => self.note("Voice", format!("{error:#}")),
            }
        }
        if self
            .speech
            .as_ref()
            .is_some_and(|(_, handle)| handle.is_finished())
        {
            changed = true;
            if let Some((_, handle)) = self.speech.take() {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => self.note("Read-out", format!("{error:#}")),
                    Err(_) => self.note("Read-out", "The read-out stopped unexpectedly."),
                }
            }
        }
        changed
    }

    /// Preserve existing drafts for review. Only a fresh speech-only draft can auto-send; a
    /// recognition result must never silently execute a slash command or shell escape.
    fn voice_transcript(&mut self, app: &App, text: &str) {
        let clean = crate::ui::text::sanitize(&text.replace("\r\n", "\n").replace('\r', "\n"));
        let draft_empty = self.composer.draft().is_empty();
        let (text, send) = submission(&clean, app.config.voice.stt.submit_trigger);
        if text.is_empty() {
            return;
        }
        self.composer.paste_voice(text);
        if send
            && draft_empty
            && !text.starts_with(['/', '!'])
            && let ComposerEvent::Submit(text) = self.composer.submit_voice()
        {
            self.deferred.push_back(text);
        }
    }

    /// Stops a read-out that is still playing. Its thread exits at the next cancel check.
    pub(super) fn stop_speech(&mut self) {
        if let Some((cancel, _)) = self.speech.take() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Reads the last answer aloud when `voice.tts.auto_speak` is on.
    pub(super) fn auto_speak(&mut self, app: &App, agent: &Agent<'_>) {
        self.stop_speech();
        if !app.config.voice.tts.auto_speak || self.voice_input.is_some() {
            return;
        }
        let messages = agent.session().messages();
        // Only this turn's messages: a tool-only turn must not re-read an earlier answer.
        let turn_start = messages
            .iter()
            .rposition(|m| m["role"] == "user")
            .map_or(0, |i| i + 1);
        let Some(answer) = messages
            .iter()
            .skip(turn_start)
            .rev()
            .filter(|m| m["role"] == "assistant")
            .find_map(|m| m["content"].as_str().filter(|s| !s.trim().is_empty()))
        else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = crate::voice::tui::speak_in_background(
            crate::voice::VoiceEnv::from_config(&app.config),
            answer.to_owned(),
            cancel.clone(),
        );
        self.speech = Some((cancel, handle));
    }
}

/// Apply the selected speech submission rule without changing text kept for manual review.
fn submission(text: &str, trigger: SubmitTrigger) -> (&str, bool) {
    let text = text.trim();
    match trigger {
        SubmitTrigger::Never => (text, false),
        SubmitTrigger::Release => (text, true),
        SubmitTrigger::ReleaseComplete => (text, text.ends_with(['.', '?', '!'])),
        SubmitTrigger::SaySubmit => {
            let words = text.trim_end_matches(['.', '?', '!']);
            let start = words
                .char_indices()
                .rfind(|(_, c)| c.is_whitespace())
                .map_or(0, |(i, c)| i + c.len_utf8());
            if words[start..].eq_ignore_ascii_case("submit") {
                (words[..start].trim_end(), true)
            } else {
                (text, false)
            }
        }
    }
}

#[cfg(test)]
mod tests;

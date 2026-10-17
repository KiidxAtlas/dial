//! Voice input and spoken replies, driven by the push-to-talk key.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::shell::Shell;
use crate::ui::keys::Action;
use crossterm::event::KeyEventKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

impl Shell {
    /// Voice key press (not an auto-repeat): start push-to-talk, or stop the capture that is
    /// running. `poll_voice` inserts the transcript into the draft once the capture finishes.
    pub(in crate::app) fn voice_press(&mut self, app: &App) {
        let release_reporting = crate::ui::term::reports_key_releases();
        match self.voice_gesture.press(release_reporting) {
            crate::voice::gesture::Command::Stop => self.stop_voice(),
            crate::voice::gesture::Command::Start { hold } => {
                if !app.config.voice.stt.enabled {
                    self.voice_gesture.finished();
                    self.note(
                        "Voice",
                        "Voice input is off. Turn on speech recognition under Voice in /settings.",
                    );
                    return;
                }
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
    pub(in crate::app) fn poll_voice(&mut self) -> bool {
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
                Ok(text) if !text.trim().is_empty() => self.composer_paste(text.trim()),
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

    /// Stops a read-out that is still playing. Its thread exits at the next cancel check.
    pub(super) fn stop_speech(&mut self) {
        if let Some((cancel, _)) = self.speech.take() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Reads the last answer aloud when `voice.tts.auto_speak` is on.
    pub(super) fn auto_speak(&mut self, app: &App, agent: &Agent<'_>) {
        self.stop_speech();
        if !app.config.voice.tts.auto_speak {
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

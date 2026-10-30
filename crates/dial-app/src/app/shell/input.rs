//! Key handling for the home screen and the composer, and external editing.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::drive::Step;
use crate::app::overlay::Which;
use crate::app::shell::Shell;
use crate::ui::composer::{ComposerCtx, ComposerEvent};
use crate::ui::input::{poll, read};
use crate::ui::keys::Action;
use crate::ui::transcript::TranscriptCtx;
use crate::ui::welcome;
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use std::process::Command;
use std::time::{Duration, Instant};

impl Shell {
    /// Keys for the live home while the input is empty: Down enters its list, Up walks back to the
    /// input, Enter picks, Esc leaves. `None` passes the key on; `Some(Some(command))` asks to run it.
    pub(super) fn home_key(&mut self, key: &crossterm::event::KeyEvent) -> Option<Option<String>> {
        if let Some(command) = self.extension_keys.key(*key, &self.keys) {
            return Some(Some(command));
        }
        if self.extension_views.key(*key, &self.keys) {
            return Some(None);
        }
        let home = self.home.as_mut()?;
        if !key.modifiers.is_empty()
            || !self.composer.draft().is_empty()
            || self.composer.menu_open()
        {
            return None;
        }
        match key.code {
            KeyCode::Down => home.down().then_some(None),
            KeyCode::Up => home.up().then_some(None),
            KeyCode::Esc => home.deselect().then_some(None),
            KeyCode::Enter => match home.pick()? {
                welcome::Pick::Resume(id) => Some(Some(format!("/resume {id}"))),
                welcome::Pick::Draft(text) => {
                    self.composer.set_draft(text);
                    Some(None)
                }
            },
            _ => {
                // Typing goes to the input; the selection steps aside.
                home.deselect();
                None
            }
        }
    }

    /// Attaches a clipboard image whose worker read has finished. True when the draft was updated.
    pub(in crate::app) fn poll_clipboard_image(&mut self) -> bool {
        match self.composer.finish_clipboard_image() {
            None => false,
            Some(Ok(())) => {
                self.note("Image", "clipboard image attached to the draft");
                true
            }
            Some(Err(error)) => {
                self.note("Image", format!("clipboard image unavailable: {error:#}"));
                true
            }
        }
    }

    /// Keep typing live while an agent's tools connect, including same-session rebuilds.
    /// Once an action is held, queue later input in order until the connection pass finishes.
    pub(in crate::app) fn edit_while(&mut self, busy: impl Fn() -> bool) {
        self.edit_while_events(busy, |frame| {
            if poll(frame)? {
                read().map(Some)
            } else {
                Ok(None)
            }
        });
    }

    fn edit_while_events(
        &mut self,
        busy: impl Fn() -> bool,
        mut next: impl FnMut(Duration) -> std::io::Result<Option<Event>>,
    ) {
        while busy() {
            let animating = self.home.as_ref().is_some_and(welcome::Home::animating);
            let frame = if animating {
                crate::ui::fx::FRAME
            } else {
                Duration::from_millis(16)
            };
            let event = match next(frame) {
                Ok(Some(event)) => event,
                Ok(None) => {
                    if animating {
                        let _ = self.paint(None);
                    }
                    continue;
                }
                Err(_) => return,
            };
            if !self.held.is_empty() {
                self.held.push_back(event);
                continue;
            }
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Release => continue,
                Event::Key(key)
                    if (key.modifiers - KeyModifiers::SHIFT).is_empty()
                        && match key.code {
                            // A plain character can be rebound to an action; those wait.
                            KeyCode::Char(_) => self.keys.actions_for(&key).is_empty(),
                            KeyCode::Backspace | KeyCode::Delete => true,
                            _ => false,
                        } =>
                {
                    // Typing deselects a picked home row, as it does in the main loop.
                    let _ = self.home_key(&key);
                    self.composer_key(key);
                }
                Event::Paste(text) => self.composer_paste(&text),
                other => self.held.push_back(other),
            }
            let _ = self.paint(None);
        }
    }

    pub(in crate::app) fn composer_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> ComposerEvent {
        if let Some(control) = &self.maintenance_control {
            control.note_input();
        }
        let buffered = poll(Duration::ZERO).unwrap_or(false);
        let guarded = self.paste_guard.text_for_draft_key(
            key,
            buffered,
            Instant::now(),
            &self.composer.draft(),
        );
        if let Some(text) = guarded {
            self.composer.handle_paste(text);
            return ComposerEvent::None;
        }
        let ctx = ComposerCtx {
            theme: self.themes.current(),
            keys: &self.keys,
            ui: &self.ui,
            status: &self.status,
        };
        self.composer.handle_key(key, &ctx)
    }

    pub(in crate::app) fn composer_paste(&mut self, text: &str) {
        if let Some(control) = &self.maintenance_control {
            control.note_input();
        }
        self.paste_guard.pasted(Instant::now());
        self.composer.handle_paste(text);
    }

    pub(super) fn on_action(
        &mut self,
        action: Action,
        app: &App,
        agent: &mut Agent<'_>,
    ) -> Result<Step> {
        match action {
            Action::CommandPalette => self.open(app, agent, Which::Palette),
            Action::SubagentsHub => self.open(app, agent, Which::Subagents),
            Action::SubagentsRecent => self.open_recent_worker(app, agent),
            Action::ModelSelect => self.open(app, agent, Which::Models),
            Action::ModelRoles => self.open(app, agent, Which::ModelRoles),
            Action::ModelFavoriteNext => self.command(app, agent, "/model next"),
            Action::RateGood | Action::RateBad => {
                self.rate_last_turn(app, action == Action::RateGood);
                Ok(Step::Done)
            }
            Action::ProviderSetup => self.open(app, agent, Which::Providers),
            Action::Settings => self.open(app, agent, Which::Settings),
            Action::ToolsExpand => {
                self.tools_expanded = !self.tools_expanded;
                self.display_reset();
                Ok(Step::Done)
            }
            Action::ToolsToggle => {
                self.ui.show_tools = !self.ui.show_tools;
                self.note(
                    "Tools",
                    if self.ui.show_tools {
                        "tool activity shown"
                    } else {
                        "tool activity hidden (errors still show)"
                    },
                );
                Ok(Step::Done)
            }
            Action::ThinkingToggle => {
                self.ui.show_thinking = !self.ui.show_thinking;
                self.note(
                    "Reasoning",
                    if self.ui.show_thinking {
                        "reasoning text shown"
                    } else {
                        "reasoning text collapsed"
                    },
                );
                Ok(Step::Done)
            }
            Action::ApprovalCycle => {
                self.cycle_approval(agent);
                Ok(Step::Done)
            }
            Action::DisplayReset => {
                self.display_reset();
                Ok(Step::Done)
            }
            Action::EditExternal => {
                self.edit_externally();
                Ok(Step::Done)
            }
            Action::PasteImage => {
                if let Err(error) = self.composer.start_clipboard_image() {
                    self.note("Image", format!("clipboard image unavailable: {error:#}"));
                }
                Ok(Step::Done)
            }
            Action::VoiceInput => {
                self.voice_press(app);
                Ok(Step::Done)
            }
            _ => Ok(Step::Done),
        }
    }

    /// Erase the screen and scrollback, then repaint the whole conversation from the transcript.
    pub(in crate::app) fn display_reset(&mut self) {
        if self.term.clear_screen().is_err() {
            return;
        }
        let (cols, _) = self.term.size();
        let Shell {
            term,
            themes,
            transcript,
            ui,
            status,
            tools_expanded,
            keys,
            ..
        } = self;
        let expand_key = keys.label(Action::ToolsExpand);
        let ctx = TranscriptCtx {
            theme: themes.current(),
            ui,
            tick: status.tick,
            tools_expanded: *tools_expanded,
            expand_key: &expand_key,
        };
        // Only what already reached scrollback is replayed; the still-streaming tail stays live.
        let rows = transcript.committed_rows(cols, &ctx);
        term.commit(rows);
    }

    /// Edit the draft in `$VISUAL`/`$EDITOR` on the real terminal.
    pub(super) fn edit_externally(&mut self) {
        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            });
        let Some(editor) = editor else {
            self.note(
                "Editor",
                "set $VISUAL or $EDITOR to edit the draft externally",
            );
            return;
        };
        let draft_dir = match tempfile::Builder::new().prefix("dial-draft-").tempdir() {
            Ok(dir) => dir,
            Err(error) => {
                self.note(
                    "Editor",
                    format!("cannot create private draft directory: {error}"),
                );
                return;
            }
        };
        if let Err(error) = crate::platform::fs::set_owner_only(draft_dir.path()) {
            self.note("Editor", format!("cannot protect draft directory: {error}"));
            return;
        }
        let path = draft_dir.path().join("draft.md");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let draft_file = crate::platform::fs::open_owner_only(&options, &path);
        let mut draft_file = match draft_file {
            Ok(file) => file,
            Err(error) => {
                self.note(
                    "Editor",
                    format!("cannot create private draft file: {error}"),
                );
                return;
            }
        };
        if let Err(error) =
            std::io::Write::write_all(&mut draft_file, self.composer.expanded_draft().as_bytes())
        {
            let _ = std::fs::remove_file(&path);
            self.note(
                "Editor",
                format!("cannot write the draft for the editor: {error}"),
            );
            return;
        }
        drop(draft_file);
        let ran = self.term.suspend(|| run_editor(&editor, &path));
        match ran {
            Ok(Ok(status)) if status.success() => {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    self.composer.set_draft(text.trim_end_matches('\n'));
                }
            }
            Ok(Ok(status)) => self.note(
                "Editor",
                format!("editor exited with {status}; draft unchanged"),
            ),
            Ok(Err(error)) => self.note("Editor", format!("could not start the editor: {error}")),
            Err(error) => self.note("Editor", format!("terminal error: {error:#}")),
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// The editor setting may carry arguments (`code --wait`), so it goes through the shell.
#[cfg(unix)]
fn run_editor(editor: &str, path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("dial-editor")
        .arg(path)
        .status()
}

/// The editor setting may carry arguments (`code --wait`), so it goes through PowerShell.
/// PowerShell 7 (`pwsh`) is an optional install; Windows PowerShell ships with every Windows.
#[cfg(windows)]
fn run_editor(editor: &str, path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let run = |shell: &str| {
        Command::new(shell)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!("& {editor} $env:DIAL_EDITOR_FILE"))
            .env("DIAL_EDITOR_FILE", path)
            .status()
    };
    match run("pwsh") {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => run("powershell.exe"),
        other => other,
    }
}

#[cfg(test)]
mod tests;

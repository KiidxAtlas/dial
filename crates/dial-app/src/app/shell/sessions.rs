//! Session display, the session picker and resuming a worker.

use crate::agent::Agent;
use crate::app::bridge::Bridge;
use crate::app::core::App;
use crate::app::display::short;
use crate::app::replay::replay_session;
use crate::app::shell::{PAINT_EVERY, Shell};
use crate::config::paths::Layout;
use crate::storage::session::Session;
use crate::ui::text;
use crate::ui::transcript::Transcript;
use anyhow::{Context, Result};
use crossterm::event::{Event, read};
use std::process::{Command, Stdio};
use std::time::Instant;

/// The picker loads only the newest sessions; parsing every journal would block the UI thread.
/// Older sessions remain reachable by id through the direct prefix lookup.
const PICKER_SESSION_LIMIT: usize = 200;

impl Shell {
    /// Show a different session's conversation: clear the screen and replay it from the session file.
    pub(in crate::app) fn show_session(&mut self, session: &Session) {
        let _ = self.term.clear_screen();
        self.transcript = Transcript::new();
        replay_session(&mut self.transcript, session);
        self.session_id = session.id().to_owned();
    }

    pub(super) fn pick_session(
        &mut self,
        agent: &mut Agent<'_>,
        query: &str,
        tree: bool,
        direct: bool,
    ) -> Result<Option<Session>> {
        use crate::ui::session_picker::Action as Pick;
        let layout = agent
            .layout()
            .context("agent has no project layout")?
            .clone();
        let current = agent.session().id().to_owned();
        if direct && !query.is_empty() {
            let matches = Session::find_prefix(&layout, query)?;
            match matches.as_slice() {
                [only] if only.id == current => {
                    let name = Self::session_name(&only.title, &only.id);
                    self.note("Session", format!("Already in session {name}"));
                    return Ok(None);
                }
                [only] => return Session::open(&only.path).map(Some),
                // No match, or an ambiguous prefix: the picker opens with the prefix as its search.
                _ => {}
            }
        }
        let mut scope = (false, PICKER_SESSION_LIMIT);
        let show_empty = direct && !query.is_empty();
        let mut picker =
            Self::load_session_picker(&layout, &current, tree, scope, query, show_empty)?;
        if picker.sessions.is_empty() {
            self.note("Sessions", "no sessions yet");
            return Ok(None);
        }
        loop {
            let (cols, height) = self.term.size();
            if let Some((id, path)) = picker.wanted_preview(cols as usize, height as usize) {
                picker.set_preview(id, Session::preview_turns(&path).unwrap_or_default());
            }
            let rows = picker.rows(cols as usize, height as usize, self.themes.current());
            self.paint(Some(rows))?;
            let event = read()?;
            if matches!(event, Event::Resize(..)) {
                self.term.on_resize();
            }
            match picker.handle(&event, height as usize) {
                Pick::Cancel => return Ok(None),
                Pick::Resume(index) => {
                    let session = &picker.sessions[index];
                    let (id, path) = (session.id.clone(), session.path.clone());
                    let name = Self::session_name(&session.title, &id);
                    if id == current {
                        self.note("Session", format!("Already in session {name}"));
                        return Ok(None);
                    }
                    if path.parent() != Some(layout.sessions_dir().as_path()) {
                        let project = session.project.clone();
                        picker.set_notice(format!(
                            "{name} belongs to {project}. Run dial there to resume it, or /move it here."
                        ));
                        continue;
                    }
                    match Session::open(&path) {
                        Ok(session) => return Ok(Some(session)),
                        // The picker stays open so another session can be chosen.
                        Err(error) => picker.set_notice(format!("{error:#}")),
                    }
                }
                Pick::Pin(index) => {
                    let id = picker.sessions[index].id.clone();
                    match crate::storage::session_pins::toggle(&layout.user_dir, &id) {
                        Ok(pinned) => picker.set_pinned(index, pinned),
                        Err(error) => picker.set_notice(format!("{error:#}")),
                    }
                }
                Pick::Rename(index, title) => {
                    let (id, path) = {
                        let session = &picker.sessions[index];
                        (session.id.clone(), session.path.clone())
                    };
                    let result = if id == current {
                        agent.session_mut().rename(&title)
                    } else {
                        Session::open(&path).and_then(|mut session| session.rename(&title))
                    };
                    let notice = match result {
                        Ok(()) => format!("Renamed to {title}."),
                        Err(error) => format!("Rename failed: {error:#}"),
                    };
                    Self::reload_session_picker(
                        &mut picker,
                        &layout,
                        &current,
                        tree,
                        scope,
                        Some(notice),
                    )?;
                }
                Pick::Fork(index) => {
                    let (id, path, name) = {
                        let session = &picker.sessions[index];
                        (
                            session.id.clone(),
                            session.path.clone(),
                            Self::session_name(&session.title, &session.id),
                        )
                    };
                    let forked = if id == current {
                        agent.session().fork(&layout)
                    } else {
                        Session::read(&path).and_then(|source| source.fork(&layout))
                    };
                    let notice = match forked {
                        Ok(fork) => format!("Forked {name} as {}.", short(fork.id())),
                        Err(error) => format!("Fork failed: {error:#}"),
                    };
                    Self::reload_session_picker(
                        &mut picker,
                        &layout,
                        &current,
                        tree,
                        scope,
                        Some(notice),
                    )?;
                }
                Pick::Delete(index) => {
                    let (id, path, name) = {
                        let session = &picker.sessions[index];
                        (
                            session.id.clone(),
                            session.path.clone(),
                            Self::session_name(&session.title, &session.id),
                        )
                    };
                    if id == current {
                        picker.set_notice("Use /delete to delete the current session.");
                        continue;
                    }
                    let body = format!("Delete {name}? Its file is removed and cannot be resumed.");
                    if self.ask(
                        "Delete session?",
                        &body,
                        &[("Cancel", 'n'), ("Delete", 'd')],
                    ) != Some(1)
                    {
                        continue;
                    }
                    // The pin is dropped only after the journal is gone.
                    let removed = Session::open(&path)
                        .and_then(|session| session.remove_file())
                        .and_then(|()| crate::storage::session_pins::unpin(&layout.user_dir, &id));
                    let notice = match removed {
                        Ok(()) => format!("Deleted {name}."),
                        Err(error) => format!("Delete failed: {error:#}"),
                    };
                    Self::reload_session_picker(
                        &mut picker,
                        &layout,
                        &current,
                        tree,
                        scope,
                        Some(notice),
                    )?;
                }
                Pick::Scope => {
                    scope = (!scope.0, PICKER_SESSION_LIMIT);
                    Self::reload_session_picker(&mut picker, &layout, &current, tree, scope, None)?;
                }
                Pick::LoadAll => {
                    scope.1 = usize::MAX;
                    Self::reload_session_picker(&mut picker, &layout, &current, tree, scope, None)?;
                }
                Pick::None => {}
            }
        }
    }

    /// The resume picker for one scope `(all projects, per-kind limit)`. Pins, open-elsewhere marks
    /// and the journal count behind "showing N of M" are read here, never in the picker.
    fn load_session_picker(
        layout: &Layout,
        current: &str,
        tree: bool,
        scope: (bool, usize),
        query: &str,
        show_empty: bool,
    ) -> Result<crate::ui::session_picker::Picker> {
        let (all, limit) = scope;
        let sessions = if all {
            Session::list_for_picker_all(layout, limit)?
        } else {
            Session::list_for_picker(layout, limit)?
        };
        let total = Session::count_for_picker(layout, all)?;
        let open_elsewhere: std::collections::HashSet<String> = sessions
            .iter()
            .filter(|s| s.id != current && Session::open_elsewhere(&s.path))
            .map(|s| s.id.clone())
            .collect();
        let pinned = crate::storage::session_pins::load(&layout.user_dir)?;
        Ok(
            crate::ui::session_picker::Picker::new(sessions, pinned, current, query, tree)
                .scope(all, total)
                .with_open_elsewhere(open_elsewhere)
                .with_show_empty(show_empty),
        )
    }

    /// Rebuilds the picker for `scope`, keeping the highlighted session when it is still listed.
    fn reload_session_picker(
        picker: &mut crate::ui::session_picker::Picker,
        layout: &Layout,
        current: &str,
        tree: bool,
        scope: (bool, usize),
        notice: Option<String>,
    ) -> Result<()> {
        let keep = picker.selected_id().map(str::to_owned);
        let mut next = Self::load_session_picker(
            layout,
            current,
            tree,
            scope,
            picker.query(),
            picker.show_empty(),
        )?;
        if let Some(id) = keep {
            next.select_id(&id);
        }
        if let Some(notice) = notice {
            next.set_notice(notice);
        }
        *picker = next;
        Ok(())
    }

    /// A session's title with its short id, or the short id alone when it has no title.
    fn session_name(title: &str, id: &str) -> String {
        let title = text::one_line(title);
        if title.trim().is_empty() {
            short(id)
        } else {
            format!("\"{}\" ({})", title.trim(), short(id))
        }
    }

    /// What a resumed session reports, at startup and after `/resume`: its name, model, last
    /// activity, then the recorded state.
    pub(super) fn resume_note(session: &Session) -> String {
        let model = match session.model_name().rsplit('/').next() {
            Some(name) if !name.is_empty() => name.to_owned(),
            _ => "unknown model".to_owned(),
        };
        format!(
            "{} · {model} · active {}\n{}",
            Self::session_name(&session.title(), session.id()),
            crate::ui::session_picker::age(&session.updated_at()),
            session.resume_summary()
        )
    }

    pub(super) fn copy_answer(&mut self, agent: &Agent<'_>) {
        use std::io::Write;
        let messages = agent.session().messages();
        let Some(answer) = messages
            .iter()
            .rev()
            .filter(|m| m["role"] == "assistant")
            .find_map(|m| m["content"].as_str().filter(|s| !s.is_empty()))
        else {
            self.note("Copy", "no assistant answer yet");
            return;
        };
        for (program, args) in [
            ("pbcopy", &[][..]),
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
        ] {
            let Ok(mut child) = Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                continue;
            };
            let wrote = child
                .stdin
                .take()
                .is_some_and(|mut stdin| stdin.write_all(answer.as_bytes()).is_ok());
            if child.wait().is_ok_and(|status| status.success()) && wrote {
                self.note("Copy", "answer copied");
                return;
            }
        }
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(answer);
        let mut stdout = std::io::stdout().lock();
        let sent = write!(stdout, "\x1b]52;c;{encoded}\x07").and_then(|_| stdout.flush());
        drop(stdout);
        match sent {
            Ok(()) => self.note(
                "Copy",
                "clipboard sent via OSC52 (terminal support required)",
            ),
            Err(error) => self.note("Copy", format!("{error}")),
        }
    }

    /// Launches a finished worker's resume (`payload` is `id` then a newline and the message)
    /// through the `task` tool's resume path. It is a live job, not a model turn. Outcomes are
    /// reported as notes.
    pub(super) fn resume_worker(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        payload: &str,
    ) -> Result<()> {
        let (id, message) = payload.split_once('\n').unwrap_or((payload, ""));
        let args = serde_json::json!({"action": "resume", "id": id, "message": message});
        let (result, failed) = {
            let mut bridge = Bridge {
                app,
                shell: self,
                interrupt: app.interrupt.clone(),
                started: Instant::now(),
                last_paint: Instant::now() - PAINT_EVERY,
                failed: false,
                pending: Vec::new(),
                next_id: 0,
            };
            let result = agent.start_subagents(&args, &mut bridge);
            (result, bridge.failed)
        };
        if failed {
            anyhow::bail!("terminal draw failed");
        }
        match result {
            Ok(_) => self.note("Subagents", format!("Resumed {id}.")),
            Err(error) => self.note("Subagents", format!("Cannot resume {id}: {error:#}")),
        }
        Ok(())
    }
}

//! Actionable full-text results reuse normal resume; inspection never selects a branch.
use super::{Shell, sessions::open_resumed};
use crate::agent::Agent;
use crate::app::{core::App, drive::Step, overlay::Which};
use crate::config::paths::Layout;
use crate::storage::session::{Session, SessionInfo};
use anyhow::{Context, Result, bail, ensure};

/// IDs are labels resolved through the project's catalogue, never interpolated into paths.
fn resolve(layout: &Layout, id: &str) -> Result<SessionInfo> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Use a session ID or unambiguous ID prefix, not a path"
    );
    let matches = Session::find_prefix(layout, id)?;
    let found = match matches.as_slice() {
        [only] => only,
        [] => bail!("No conversation with ID {id} in this project"),
        _ => match matches.iter().find(|session| session.id == id) {
            Some(exact) => exact,
            None => bail!("Session ID {id} is ambiguous; use a longer ID"),
        },
    };
    ensure!(
        found.path.parent() == Some(layout.sessions_dir().as_path()),
        "Conversation belongs to another project; resume it there or /move it here"
    );
    Ok(found.clone())
}

impl Shell {
    pub(super) fn search_command(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        args: &str,
    ) -> Result<Step> {
        let args = args.trim();
        if args == "results" {
            return match self.take_search_panel() {
                Some(panel) => self.open(app, agent, Which::HistorySearch(panel)),
                None => {
                    self.note(
                        "Search",
                        "No ready results. Use /search <query> and wait for the search to finish.",
                    );
                    Ok(Step::Done)
                }
            };
        }
        let back = args == "back";
        if back || args == "open" || args.starts_with("open ") {
            let target = if back {
                self.search.origin().map(str::to_owned)
            } else {
                Some(
                    args.strip_prefix("open")
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                )
            };
            let Some(target) = target else {
                self.note("Search", "No search-origin conversation to return to.");
                return Ok(Step::Done);
            };
            if let Err(error) = self.navigate_search(app, agent, &target, back) {
                self.note("Search", format!("Cannot open conversation: {error:#}"));
            }
            return Ok(Step::Done);
        }
        if args.is_empty() {
            self.note(
                "Search",
                "Usage: /search <query> | /search results | /search open <id> | /search back",
            );
        } else {
            self.start_search(agent.layout().unwrap_or(&app.layout), args);
            self.note("Search", "Searching past messages…");
        }
        Ok(Step::Done)
    }

    fn navigate_search(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        target: &str,
        back: bool,
    ) -> Result<()> {
        let layout = agent
            .layout()
            .context("agent has no project layout")?
            .clone();
        ensure!(
            layout.state_dir == app.layout.state_dir,
            "Agent and shell project differ; reload before navigating history"
        );
        let destination = resolve(&layout, target)?;
        if destination.id == agent.session().id() {
            if back {
                self.poll_input_recovery(app);
                self.restore_input_recovery_if_empty();
                self.search.clear_origin();
            } else {
                self.note(
                    "Search",
                    "Already in this conversation; the match preview does not rewind it.",
                );
            }
            return Ok(());
        }
        agent
            .gate_session_change(
                "session_before_switch",
                serde_json::json!({
                    "reason": "resume", "target_session_id": destination.id,
                }),
            )
            .map_err(anyhow::Error::msg)?;
        self.poll_input_recovery(app);
        self.prepare_input_session_change()?;
        let (session, forked) = open_resumed(&layout, &destination.path)?;
        let origin = agent.session().id().to_owned();
        // Bind the source before its change hook checkpoints and clears conversation-local input.
        self.poll_input_recovery(app);
        self.swap_agent_session(app, agent, session)?;
        if !back {
            self.search.remember_origin(&origin);
        }
        self.show_session(agent.session());
        agent.set_plan_mode(false);
        self.stop_loop_for_session_change();
        self.poll_input_recovery(app);
        if back {
            self.restore_input_recovery_if_empty();
            self.search.clear_origin();
        }
        if let Some(source) = forked {
            self.note(
                "Search",
                format!(
                    "Conversation is open elsewhere; resumed as an independent fork of {source}."
                ),
            );
        }
        self.note("Resume", Self::resume_note(agent.session()));
        Ok(())
    }
}

#[cfg(test)]
mod project_tests;
#[cfg(test)]
mod tests;

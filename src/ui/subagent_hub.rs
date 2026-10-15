//! Controller for the subagent hub: throttled snapshots of the session's workers and the actions
//! that act on them. The panel is [`crate::ui::overlay::subagents::SubagentsOverlay`]; the data
//! comes from [`crate::ui::subagent_model`]. Reads run here, on the overlay tick, never in draw.
use std::time::{Duration, Instant};

use crossterm::event::Event;
use ratatui::Frame;

use crate::{
    agent::LiveTaskControl,
    llm::model_config::{RoleSource, RoleStatus},
    runtime::Tree,
    storage::paths::Layout,
    ui::{
        keys::Keymap,
        overlay::{DrawCtx, OverlayEvent, subagents::SubagentsOverlay},
        subagent_model::{self, ActionOutcome, HubSnapshot},
    },
};

/// How often the roster and activity feed are re-read. Drawing reuses the last snapshot.
const REFRESH_EVERY: Duration = Duration::from_secs(1);

pub struct SubagentHub {
    session: String,
    panel: SubagentsOverlay,
    snapshot_at: Option<Instant>,
}

impl SubagentHub {
    pub fn new(session: String) -> Self {
        Self {
            session,
            panel: SubagentsOverlay::new(),
            snapshot_at: None,
        }
    }

    /// Opens the hub on the Activity tab (`subagent-hub:activity`).
    pub fn open_activity(session: String) -> Self {
        let mut hub = Self::new(session);
        hub.panel.show_activity();
        hub
    }

    /// Re-reads the workers when the interval has elapsed. Called from the overlay tick; never
    /// from draw. Role routes are refreshed on every call because they are cheap.
    pub fn refresh(
        &mut self,
        layout: &Layout,
        now: Instant,
        runtime: &Tree,
        roles: &[RoleStatus],
        live: Option<&LiveTaskControl>,
    ) {
        self.panel.set_routes(
            roles
                .iter()
                .filter(|role| matches!(role.role, "main" | "small" | "tester" | "planner"))
                .map(|role| {
                    format!(
                        " {} → {}{}",
                        role.role,
                        role.effective.as_deref().unwrap_or("unset"),
                        if role.source == RoleSource::FollowsMain {
                            " (follows main)"
                        } else {
                            ""
                        }
                    )
                })
                .collect(),
        );
        if self
            .snapshot_at
            .is_none_or(|at| now.duration_since(at) >= REFRESH_EVERY)
        {
            self.snapshot_at = Some(now);
            let snapshot = subagent_model::snapshot(layout, &self.session, runtime, live);
            self.panel.set_snapshot(snapshot);
        }
    }

    /// Replaces the snapshot directly (tests and callers that already hold one).
    pub fn set_snapshot(&mut self, snapshot: HubSnapshot) {
        self.panel.set_snapshot(snapshot);
    }

    /// Runs a `subagent-*` action. Returns an action for the app to launch (a finished worker
    /// resumed with a message); banners carry every other outcome.
    pub fn act(&mut self, action: &str, live: Option<&LiveTaskControl>) -> Option<String> {
        match subagent_model::act(action, live)? {
            Ok(ActionOutcome::Banner(message)) => {
                self.panel.banner(message, false);
                None
            }
            Ok(ActionOutcome::Resume { id, message }) => {
                Some(format!("subagent-resume:{id}\n{message}"))
            }
            Err(error) => {
                self.panel.banner(format!("{error:#}"), true);
                None
            }
        }
    }

    pub fn banner(&mut self, message: String, is_error: bool) {
        self.panel.banner(message, is_error);
    }

    /// Left click on a row; see [`SubagentsOverlay::click`].
    pub fn click(&mut self, col: u16, row: u16, now: Instant) -> OverlayEvent {
        self.panel.click(col, row, now)
    }

    /// Wheel over the inspector scrolls it; see [`SubagentsOverlay::wheel_over_inspector`].
    pub fn wheel_over_inspector(&mut self, col: u16, row: u16, down: bool) -> bool {
        self.panel.wheel_over_inspector(col, row, down)
    }

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        self.panel.handle(event, keys)
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        self.panel.draw(frame, ctx);
    }

    /// Workers currently shown, in display order. Used by tests and the activity feed.
    pub fn agent_ids(&self) -> Vec<String> {
        self.panel.agents().iter().map(|a| a.id.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::paths::Layout;
    use crate::ui::subagent_model::{AgentEntry, AgentStatus};
    use std::time::Duration;

    fn layout() -> (tempfile::TempDir, tempfile::TempDir, Layout) {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let layout = Layout::with_user_dir(project.path(), home.path().to_path_buf()).unwrap();
        (home, project, layout)
    }

    #[test]
    fn empty_session_publishes_an_empty_roster() {
        let (_home, _project, layout) = layout();
        let mut hub = SubagentHub::new("session".into());
        hub.refresh(&layout, Instant::now(), &Tree::default(), &[], None);
        assert!(hub.agent_ids().is_empty());
    }

    #[test]
    fn snapshot_is_throttled_to_the_refresh_interval() {
        let (_home, _project, layout) = layout();
        let mut hub = SubagentHub::new("session".into());
        let at = Instant::now();
        hub.refresh(&layout, at, &Tree::default(), &[], None);
        hub.set_snapshot(HubSnapshot {
            agents: vec![AgentEntry {
                id: "task-1".into(),
                status: AgentStatus::Running,
                ..Default::default()
            }],
            ..Default::default()
        });
        // Within the interval the snapshot set above survives a refresh.
        hub.refresh(
            &layout,
            at + Duration::from_millis(400),
            &Tree::default(),
            &[],
            None,
        );
        assert_eq!(hub.agent_ids(), vec!["task-1".to_owned()]);
        // After the interval the real (empty) snapshot replaces it.
        hub.refresh(
            &layout,
            at + Duration::from_secs(2),
            &Tree::default(),
            &[],
            None,
        );
        assert!(hub.agent_ids().is_empty());
    }

    #[test]
    fn cancel_without_live_control_reports_why() {
        let mut hub = SubagentHub::new("session".into());
        assert_eq!(hub.act("subagent-cancel:task-1", None), None);
        let error = hub.panel.banner_text().expect("banner set");
        assert!(error.contains("no live subagents"), "{error}");
    }

    #[test]
    fn hub_navigation_actions_are_not_swallowed() {
        let mut hub = SubagentHub::new("session".into());
        assert_eq!(hub.act("subagent-open:task-1", None), None);
        assert!(hub.panel.banner_text().is_none());
    }
}

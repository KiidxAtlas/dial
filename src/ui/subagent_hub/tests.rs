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

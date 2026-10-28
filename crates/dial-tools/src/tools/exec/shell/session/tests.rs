use super::*;
use crate::embedded_shell::state::State;

fn workspace() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, ws)
}

#[test]
fn admit_refuses_a_session_a_background_job_holds() {
    let (_dir, ws) = workspace();
    busy_map(&ws).insert(
        "s".to_owned(),
        SessionBusy::Running {
            job: "bg_3".to_owned(),
        },
    );
    let mut sessions = BTreeMap::new();
    assert_eq!(
        admit(&ws, &mut sessions, "s").unwrap_err(),
        "session busy in background job bg_3; wait or kill it"
    );
    assert!(busy_map(&ws).contains_key("s"));
}

#[test]
fn a_discarded_session_is_removed_on_next_use() {
    let (dir, ws) = workspace();
    busy_map(&ws).insert("s".to_owned(), SessionBusy::Settled(None));
    let mut sessions = BTreeMap::new();
    sessions.insert("s".to_owned(), State::new(dir.path()));
    admit(&ws, &mut sessions, "s").unwrap();
    assert!(!sessions.contains_key("s"));
    assert!(!busy_map(&ws).contains_key("s"));
}

#[test]
fn a_published_session_replaces_the_stored_state_on_next_use() {
    let (dir, ws) = workspace();
    let published = State::new(dir.path());
    busy_map(&ws).insert("s".to_owned(), SessionBusy::Settled(Some(published)));
    let mut sessions = BTreeMap::new();
    admit(&ws, &mut sessions, "s").unwrap();
    assert_eq!(sessions["s"].cwd, dir.path());
    assert!(!busy_map(&ws).contains_key("s"));
}

#[test]
fn an_unsettled_claim_discards_the_session_when_dropped() {
    let (dir, ws) = workspace();
    let exchange = Exchange::new(dir.path(), &State::new(dir.path())).unwrap();
    let mut claim = SessionClaim::new(&ws, "s", exchange);
    claim.mark("bg_1");
    assert!(matches!(
        busy_map(&ws).get("s"),
        Some(SessionBusy::Running { .. })
    ));
    drop(claim);
    assert!(matches!(
        busy_map(&ws).get("s"),
        Some(SessionBusy::Settled(None))
    ));
}

#[test]
fn a_claim_that_settles_unclean_discards_the_session() {
    let (dir, ws) = workspace();
    let exchange = Exchange::new(dir.path(), &State::new(dir.path())).unwrap();
    let mut claim = SessionClaim::new(&ws, "s", exchange);
    claim.mark("bg_2");
    claim.settle(false);
    assert!(matches!(
        busy_map(&ws).get("s"),
        Some(SessionBusy::Settled(None))
    ));
}

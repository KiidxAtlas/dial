use super::*;
use crate::config::paths::Layout;
use crate::tools::Workspace;
use std::{
    fs,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Codebase) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join("routing.rs"), "pub fn route() {}\n").unwrap();
    fs::write(project.path().join("consumer.rs"), "use crate::routing;\n").unwrap();
    let layout = Layout::with_user_dir(project.path(), home.path().join("state")).unwrap();
    let cache = Codebase::open(&layout).unwrap();
    cache
        .refresh(
            &Workspace::new(project.path()).unwrap(),
            &AtomicBool::new(false),
        )
        .unwrap();
    (home, project, cache)
}

fn stamp() -> Stamp {
    Stamp {
        revision: [1, 0, 0],
        instruction: 1,
    }
}

#[test]
fn ready_hints_are_polled_once_without_waiting() {
    let (_home, _project, cache) = fixture();
    let mut hints = Enrichment::default();
    assert!(hints.submit(&cache, vec!["routing.rs".into()], stamp()));
    assert!(!hints.submit(&cache, vec!["routing.rs".into()], stamp()));
    let deadline = Instant::now() + Duration::from_secs(2);
    let text = loop {
        if let Some(text) = hints.poll(stamp()) {
            break text;
        }
        assert!(Instant::now() < deadline, "worker did not return hints");
        std::thread::yield_now();
    };
    assert!(text.contains("consumer.rs"));
    assert!(hints.poll(stamp()).is_none());
    assert!(hints.pending.is_none());
}

#[test]
fn every_effect_component_and_steering_change_discards_pending_hints() {
    let (_home, _project, cache) = fixture();
    for changed in [
        Stamp {
            revision: [2, 0, 0],
            ..stamp()
        },
        Stamp {
            revision: [1, 1, 0],
            ..stamp()
        },
        Stamp {
            revision: [1, 0, 1],
            ..stamp()
        },
        Stamp {
            instruction: 2,
            ..stamp()
        },
    ] {
        let mut hints = Enrichment::default();
        // Cancellation is cooperative; wait only in this test for the previous worker to release
        // admission. Production submit always returns immediately when another job is busy.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !hints.submit(&cache, vec!["routing.rs".into()], stamp()) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(hints.poll(changed).is_none());
        assert!(hints.pending.is_none());
    }
}

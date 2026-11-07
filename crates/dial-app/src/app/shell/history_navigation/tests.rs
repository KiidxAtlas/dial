use super::*;
use crate::llm::provider::{Backend, ProviderKind};
use crate::storage::session::SessionMeta;
use std::fs;

fn session(app: &App, text: &str) -> Session {
    let mut session = Session::create(
        &app.layout,
        SessionMeta {
            model: "fixture".into(),
        },
    )
    .unwrap();
    session
        .append(serde_json::json!({"role":"user", "content":text}))
        .unwrap();
    session
}

fn agent(app: &App, session: Session) -> Agent<'_> {
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "fixture".into(),
            api_key: None,
        },
        "unused-python".into(),
        1,
    );
    Agent::interactive(
        &app.config,
        backend,
        app.layout.clone(),
        session,
        None,
        None,
        app.interrupt.clone(),
    )
    .unwrap()
}

struct JournalBaseline {
    bytes: Vec<u8>,
    id: String,
    messages: Vec<serde_json::Value>,
    user_entries: Vec<(String, String)>,
    leaf: Option<String>,
}

impl JournalBaseline {
    fn capture(path: &std::path::Path) -> Self {
        let session = Session::read(path).unwrap();
        Self {
            bytes: fs::read(path).unwrap(),
            id: session.id().to_owned(),
            messages: session.messages(),
            user_entries: session.branchable_user_messages(),
            leaf: session.branch_leaf().map(str::to_owned),
        }
    }

    fn assert_only_resume_control_appends(&self, path: &std::path::Path) {
        let bytes = fs::read(path).unwrap();
        assert!(
            bytes.starts_with(&self.bytes),
            "resume rewrote original journal bytes: {path:?}"
        );
        let mut head = self.leaf.clone();
        for line in bytes[self.bytes.len()..]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: serde_json::Value = serde_json::from_slice(line).unwrap();
            assert_eq!(
                record["type"], "message",
                "unexpected branch/history record: {record}"
            );
            // Session::set_control_state journals equal publications as an empty patch.
            // Do not allow other metadata, human messages, or any branch marker here.
            assert_eq!(
                record["message"],
                serde_json::json!({"role":"dial_state", "patch":{}})
            );
            assert_eq!(
                record["parent"],
                serde_json::json!(head),
                "resume must extend the existing branch"
            );
            let next = record["id"].as_str().unwrap().to_owned();
            assert!(!next.is_empty());
            assert_ne!(head.as_deref(), Some(next.as_str()));
            head = Some(next);
        }
        let session = Session::read(path).unwrap();
        assert_eq!(session.id(), self.id);
        assert_eq!(
            session.messages(),
            self.messages,
            "visible conversation changed"
        );
        assert_eq!(
            session.branchable_user_messages(),
            self.user_entries,
            "human branch head or stable entry IDs changed"
        );
        if let Some(leaf) = &self.leaf {
            assert!(
                session.contains_branch_leaf(leaf),
                "original branch ancestry was abandoned"
            );
        }
        assert_eq!(
            session.branch_leaf(),
            head.as_deref(),
            "resume selected a different branch head"
        );
    }
}

#[test]
fn open_multiple_hits_and_return_preserve_origin_draft_and_human_history() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    let origin = session(&app, "origin turn");
    let origin_id = origin.id().to_owned();
    let second = session(&app, "second turn");
    let second_id = second.id().to_owned();
    let third = session(&app, "third turn");
    let third_id = third.id().to_owned();
    let paths = [
        origin.path().unwrap().to_owned(),
        second.path().unwrap().to_owned(),
        third.path().unwrap().to_owned(),
    ];
    // Agent startup binds provider identity and records its main route. Those writes belong
    // before the navigation baseline, for every conversation (not only the origin).
    drop(agent(&app, second));
    drop(agent(&app, third));
    let mut agent = agent(&app, origin);
    let before: Vec<_> = paths
        .iter()
        .map(|path| JournalBaseline::capture(path))
        .collect();
    shell.set_session_id(&origin_id);
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("original unsent text");
    shell
        .search_command(&app, &mut agent, &format!("open {second_id}"))
        .unwrap();
    assert_eq!(agent.session().id(), second_id);
    assert!(shell.composer.draft().is_empty());
    shell.composer.set_draft("second conversation draft");
    shell
        .search_command(&app, &mut agent, &format!("open {third_id}"))
        .unwrap();
    assert_eq!(shell.search.origin(), Some(origin_id.as_str()));
    shell.search_command(&app, &mut agent, "back").unwrap();
    assert_eq!(agent.session().id(), origin_id);
    assert_eq!(shell.composer.draft(), "original unsent text");
    assert!(shell.search.origin().is_none());
    assert!(shell.composer.take_follow_up().is_none());
    for (path, baseline) in paths.iter().zip(before) {
        baseline.assert_only_resume_control_appends(path);
    }
}

#[test]
fn refused_targets_and_current_session_leave_origin_input_and_journal_unchanged() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    let current = session(&app, "current");
    let id = current.id().to_owned();
    let path = current.path().unwrap().to_owned();
    let mut agent = agent(&app, current);
    // Initialization is allowed to journal provider identity and model-route receipts;
    // rejected navigation and choosing the current session must append nothing at all.
    let before = fs::read(&path).unwrap();
    shell.set_session_id(&id);
    shell.poll_input_recovery(&app);
    shell.composer.set_draft("do not replace");
    for command in [
        "open ../outside",
        "open /tmp/file",
        "open missing-id",
        "open",
        "back",
        "results",
    ] {
        shell.search_command(&app, &mut agent, command).unwrap();
        assert_eq!(agent.session().id(), id);
        assert_eq!(shell.composer.draft(), "do not replace");
        assert!(shell.search.origin().is_none());
    }
    shell
        .search_command(&app, &mut agent, &format!("open {id}"))
        .unwrap();
    assert_eq!(shell.composer.draft(), "do not replace");
    assert!(shell.search.origin().is_none());
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn busy_writer_opens_independent_fork_without_mutating_source() {
    let (_home, _project, app, mut shell) = super::super::testkit::shell();
    let current = session(&app, "current");
    let origin = current.id().to_owned();
    let held = session(&app, "held elsewhere");
    let path = held.path().unwrap().to_owned();
    let before = fs::read(&path).unwrap();
    let mut agent = agent(&app, current);
    shell.set_session_id(&origin);
    shell
        .search_command(&app, &mut agent, &format!("open {}", held.id()))
        .unwrap();
    assert_ne!(agent.session().id(), held.id());
    assert_ne!(agent.session().id(), origin);
    assert_eq!(shell.search.origin(), Some(origin.as_str()));
    assert_eq!(fs::read(path).unwrap(), before);
}

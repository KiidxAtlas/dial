use super::*;

pub(super) fn with_agent(overlay: &str, f: impl FnOnce(&mut Agent<'_>)) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let layout = Layout::with_user_dir(&project, temp.path().join("home")).unwrap();
    let overlay = toml::from_str(&format!(
        "model='test'\n{overlay}\n[improve]\nenabled=false\n[memory]\nbackend='off'\n"
    ))
    .unwrap();
    let config = Config::load(&layout, Some(overlay)).unwrap();
    let backend = Backend::new(
        ProviderKind::Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test".into(),
            api_key: None,
        },
        PathBuf::new(),
        5,
    );
    let session = Session::create(
        &layout,
        session::SessionMeta {
            model: "test".into(),
        },
    )
    .unwrap();
    let mut agent = Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        None,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    f(&mut agent);
}

#[test]
fn foreground_registration_claims_delivery_and_cleanup_share_one_owner() {
    with_agent("", |agent| {
        let layout = agent.layout().unwrap().clone();
        let peer = Participant::join(&layout, "other-session").unwrap();
        peer.publish("update client", "working", &["src/client".into()])
            .unwrap();
        let listed = agent.peer_coordination(&json!({}));
        assert!(!listed.is_error, "{}", listed.text);
        let data = listed.data.unwrap();
        let own_id = data["id"].as_str().unwrap().to_owned();
        assert_eq!(data["peers"][0]["id"], peer.id());
        let claimed = agent.peer_coordination(
            &json!({"action":"claim", "scopes":["src"], "summary":"change API"}),
        );
        assert!(!claimed.is_error, "{}", claimed.text);
        assert_eq!(claimed.data.unwrap()["overlaps"][0]["id"], peer.id());
        agent.coordination_phase("working", Some("work on API"));
        assert_eq!(peer.peers().unwrap()[0].scopes, ["src"]);
        let sent = agent.peer_coordination(
            &json!({"action":"send","to":peer.id(),"text":"preserve error variants"}),
        );
        assert_eq!(sent.data.unwrap()["status"], "queued");
        assert_eq!(peer.drain().unwrap()[0].text, "preserve error variants");
        peer.send(&own_id, "Do not treat this as permission")
            .unwrap();
        let instruction = agent.session.control_state()["current_instruction"].clone();
        let history_len = agent.session.messages().len();
        let notes = agent.poll_coordination(&mut NullUi);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["dial_peer_coordination"], true);
        assert!(
            notes[0]["content"]
                .as_str()
                .unwrap()
                .contains("Do not treat this as permission")
        );
        assert_eq!(agent.session.messages().len(), history_len);
        assert_eq!(
            agent.session.control_state()["current_instruction"],
            instruction
        );
        assert!(agent.poll_coordination(&mut NullUi).is_empty());
        agent.coordination_phase("idle", None);
        assert!(peer.peers().unwrap()[0].scopes.is_empty());
        drop(peer);
        assert_eq!(agent.poll_coordination(&mut NullUi).len(), 1);
        assert!(agent.poll_coordination(&mut NullUi).is_empty());
    });
}

#[test]
fn disabled_coordination_is_absent_even_when_explicitly_loaded() {
    with_agent("[coordination]\nenabled=false", |agent| {
        assert!(!agent.catalog().iter().any(|name| name == "peers"));
        assert!(!agent.offered().iter().any(|name| name == "peers"));
        assert!(agent.peer_coordination(&json!({})).is_error);
        assert!(agent.poll_coordination(&mut NullUi).is_empty());
    });
}

#[test]
fn coordination_never_enters_worker_offer_or_changes_stable_prefix() {
    with_agent("[assistance]\nlean_local=false", |agent| {
        assert!(agent.offered().iter().any(|name| name == "peers"));
        assert!(
            !agent
                .offered_for_children()
                .iter()
                .any(|name| name == "peers")
        );
        let before = serde_json::to_string(&agent.system_messages()).unwrap();
        let peer = Participant::join(agent.layout().unwrap(), "peer").unwrap();
        agent.poll_coordination(&mut NullUi);
        peer.publish("new intent", "working", &["src".into()])
            .unwrap();
        agent.poll_coordination(&mut NullUi);
        agent.rebuild_system();
        assert_eq!(
            serde_json::to_string(&agent.system_messages()).unwrap(),
            before
        );
    });
}

#[test]
fn invalid_claim_does_not_replace_published_intent() {
    with_agent("", |agent| {
        assert!(
            !agent
                .peer_coordination(&json!({"action":"claim","scopes":["src"]}))
                .is_error
        );
        assert!(
            agent
                .peer_coordination(&json!({"action":"claim","scopes":["../escape"]}))
                .is_error
        );
        assert_eq!(
            agent.peer_coordination(&json!({})).data.unwrap()["scopes"],
            json!(["src"])
        );
        assert!(
            agent
                .peer_coordination(&json!({"action":"send","to":"unknown","text":"hello"}))
                .is_error
        );
        assert!(
            agent
                .peer_coordination(&json!({"action":"send","to":"all","text":"hello"}))
                .is_error
        );
    });
}

#[test]
fn switching_sessions_retires_the_old_peer_identity() {
    with_agent("", |agent| {
        let layout = agent.layout().unwrap().clone();
        let observer = Participant::join(&layout, "observer").unwrap();
        let old = agent.peer_coordination(&json!({})).data.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let session = Session::create(
            &layout,
            session::SessionMeta {
                model: "test".into(),
            },
        )
        .unwrap();
        let next_session_id = session.id().to_owned();
        let _old_journal = agent.swap_session(session);
        let peers = observer.peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert_ne!(peers[0].id, old);
        assert_eq!(peers[0].session_id, next_session_id);
        assert!(observer.send(&old, "stale recipient").is_err());
    });
}

#[test]
fn scope_overlap_is_component_based() {
    assert!(overlap("src", "src/auth.rs"));
    assert!(overlap("src/auth.rs", "src"));
    assert!(!overlap("src/auth", "src/authorize"));
    assert!(!overlap("src", "src2"));
}

#[test]
fn rebuilding_same_session_keeps_identity_claims_and_queued_mail() {
    with_agent("", |agent| {
        let layout = agent.layout().unwrap().clone();
        let observer = Participant::join(&layout, "observer").unwrap();
        agent.peer_coordination(&json!({"action":"claim","scopes":["src"]}));
        let old = observer.peers().unwrap()[0].id.clone();
        let carry = agent.take_coordination().unwrap();
        observer.send(&old, "queued during rebuild").unwrap();
        let session = std::mem::replace(agent.session_mut(), Session::ephemeral());
        let backend = Backend::new(
            ProviderKind::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(),
                model: "test".into(),
                api_key: None,
            },
            PathBuf::new(),
            5,
        );
        let mut next = Agent::interactive(
            agent.config,
            backend,
            layout,
            session,
            None,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        next.restore_coordination(carry);
        let peers = observer.peers().unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].id, old);
        assert_eq!(peers[0].scopes, ["src"]);
        let notes = next.poll_coordination(&mut NullUi);
        assert!(
            notes[0]["content"]
                .as_str()
                .unwrap()
                .contains("queued during rebuild")
        );
        assert!(next.poll_coordination(&mut NullUi).is_empty());
    });
}

#[test]
fn coordination_carry_cannot_cross_project_or_session() {
    with_agent("", |agent| {
        let mut carry = agent.take_coordination().unwrap();
        carry.root = carry.root.join("another-project");
        agent.restore_coordination(carry);
        assert!(agent.peer_state.participant.is_none());
        agent.initialize_coordination();
        let mut carry = agent.take_coordination().unwrap();
        carry.state.session_id = "another-session".into();
        agent.restore_coordination(carry);
        assert!(agent.peer_state.participant.is_none());
    });
}

#[test]
fn transient_registration_failure_can_retry_without_restart() {
    with_agent("", |agent| {
        agent.peer_state.participant = None;
        let dir = agent.layout().unwrap().state_dir.join("coordination");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, "temporarily unavailable").unwrap();
        agent.initialize_coordination();
        assert!(agent.peer_state.participant.is_none());
        assert!(agent.peer_state.error.is_some());
        std::fs::remove_file(&dir).unwrap();
        agent.initialize_coordination();
        assert!(
            agent.peer_state.participant.is_none(),
            "automatic retry must back off"
        );
        assert!(!agent.peer_coordination(&json!({})).is_error);
        assert!(agent.peer_state.error.is_none());
        agent.peer_state.participant = None;
        agent.peer_state.retry_at = Some(Instant::now() - Duration::from_secs(1));
        agent.initialize_coordination();
        assert!(
            agent.peer_state.participant.is_some(),
            "expired backoff must permit automatic recovery"
        );
    });
}

#[test]
fn external_observations_notify_without_advancing_read_authority() {
    with_agent("", |agent| {
        let peer = Participant::join(agent.layout().unwrap(), "observer").unwrap();
        let path = agent.workspace.root().join("example.txt");
        std::fs::write(&path, "old\n").unwrap();
        let read = tools::execute(&agent.workspace, "read", &json!({"path":"example.txt"}), 5);
        assert!(!read.is_error, "{}", read.text);
        agent.poll_coordination(&mut NullUi);
        std::fs::write(&path, "new\n").unwrap();
        let notes = agent.poll_coordination(&mut NullUi);
        assert!(
            notes[0]["content"]
                .as_str()
                .unwrap()
                .contains("example.txt")
        );
        assert!(agent.poll_coordination(&mut NullUi).is_empty());
        let write = tools::execute(
            &agent.workspace,
            "write",
            &json!({"path":"example.txt","content":"replacement\n"}),
            5,
        );
        assert!(
            write.is_error,
            "a change notice must not grant a fresh write witness"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "new\n");
        drop(peer);
    });
}

use super::*;

/// A state bound to session `s1` with one open thread whose request has `events` queued.
fn streaming(
    session: &str,
    events: Vec<Event>,
) -> (SideThreadState, Arc<AtomicBool>, mpsc::Sender<Event>) {
    let mut state = SideThreadState::default();
    state.session = Some("s1".to_string());
    let thread = state.threads.create(None);
    state.open = Some(thread.clone());
    let (tx, events_rx) = mpsc::channel();
    for event in events {
        tx.send(event).expect("receiver is alive");
    }
    let cancel = Arc::new(AtomicBool::new(false));
    state.job = Some(Job {
        session: session.to_string(),
        thread,
        question: "why?".to_string(),
        cancel: cancel.clone(),
        events: events_rx,
        streamed: String::new(),
    });
    (state, cancel, tx)
}

#[test]
fn answered_request_is_recorded_for_its_session() {
    let (mut state, _, _events) = streaming(
        "s1",
        vec![
            Event::Text("he".to_string()),
            Event::Text("llo".to_string()),
            Event::Done(Ok("hello".to_string())),
        ],
    );
    let notes = state.drain(Some("s1"));
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].text, "hello");
    assert!(!state.busy());
    let thread = state.threads.thread("1").expect("thread exists");
    assert_eq!(thread.exchanges.len(), 1);
    assert_eq!(thread.exchanges[0].outcome, Outcome::Answered);
    assert_eq!(thread.title, "why?");
}

#[test]
fn late_result_from_another_session_is_dropped() {
    let (mut state, cancel, _events) = streaming("s0", vec![Event::Done(Ok("stale".to_string()))]);
    assert!(state.drain(Some("s1")).is_empty());
    assert!(cancel.load(Ordering::SeqCst));
    assert!(!state.busy());
    assert!(
        state
            .threads
            .thread("1")
            .expect("thread exists")
            .exchanges
            .is_empty()
    );
}

#[test]
fn cancelled_request_keeps_its_partial_answer() {
    let (mut state, cancel, _events) = streaming(
        "s1",
        vec![
            Event::Text("part".to_string()),
            Event::Done(Err("interrupted".to_string())),
        ],
    );
    cancel.store(true, Ordering::SeqCst);
    let notes = state.drain(Some("s1"));
    assert_eq!(notes.len(), 1);
    let exchange = &state.threads.thread("1").expect("thread exists").exchanges[0];
    assert_eq!(exchange.outcome, Outcome::Cancelled);
    assert_eq!(exchange.answer, "part");
}

#[test]
fn failed_request_records_the_error() {
    let (mut state, _, _events) =
        streaming("s1", vec![Event::Done(Err("provider down".to_string()))]);
    let _ = state.drain(Some("s1"));
    let exchange = &state.threads.thread("1").expect("thread exists").exchanges[0];
    assert_eq!(exchange.outcome, Outcome::Failed);
}

#[test]
fn unfinished_request_stays_running() {
    let (mut state, cancel, _events) = streaming("s1", vec![Event::Text("partial".to_string())]);
    assert!(state.drain(Some("s1")).is_empty());
    assert!(state.busy());
    assert_eq!(state.streamed().map(|(_, text)| text), Some("partial"));
    assert!(!cancel.load(Ordering::SeqCst));
}

#[test]
fn side_thread_titles_collapse_whitespace() {
    assert_eq!(title_of("  a\n  b  "), "a b");
}

#[test]
fn ending_the_main_turn_leaves_side_answers_running() {
    let (mut state, cancel, _events) = streaming("s1", vec![Event::Text("partial".to_string())]);
    state.end_turn();
    assert!(state.drain(Some("s1")).is_empty());
    assert!(state.busy());
    assert!(!cancel.load(Ordering::SeqCst));
}

#[test]
fn busy_question_without_a_turn_snapshot_is_refused() {
    let mut state = SideThreadState::default();
    assert!(state.busy_snapshot().is_none());
    assert!(state.command("hello", None).starts_with("error:"));
}

#[test]
fn promote_is_refused_outside_the_idle_path() {
    let mut state = SideThreadState::default();
    assert!(state.command("promote t1", None).starts_with("error:"));
}

#[test]
fn only_questions_wait_for_the_main_turn() {
    assert!(SideThreadState::asks("why is the build red"));
    assert!(SideThreadState::asks("new what now"));
    assert!(!SideThreadState::asks(""));
    assert!(!SideThreadState::asks("threads"));
    assert!(!SideThreadState::asks("open t1"));
    assert!(!SideThreadState::asks("promote t1"));
}

#[test]
fn cancellation_wins_when_the_provider_finished_before_the_ui_drained_its_result() {
    let (mut state, cancel, _events) = streaming(
        "s1",
        vec![Event::Done(Ok("answer completed concurrently".into()))],
    );
    cancel.store(true, Ordering::SeqCst);
    state.drain(Some("s1"));
    let exchange = &state.threads.thread("1").unwrap().exchanges[0];
    assert_eq!(exchange.outcome, Outcome::Cancelled);
    assert_eq!(exchange.answer, "answer completed concurrently");
}

#[test]
fn a_streamed_prefix_survives_owner_drop_without_reissuing_the_request() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let layout =
        crate::config::paths::Layout::with_user_dir(project.path(), home.path().join("dial"))
            .unwrap();
    let session = Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: "mock".into(),
        },
    )
    .unwrap();
    let path = side_thread::path_for(session.path().unwrap());
    let (mut state, cancel, _events) = streaming(
        session.id(),
        vec![Event::Text("durable side prefix".into())],
    );
    state.session = Some(session.id().into());
    state.path = Some(path.clone());
    assert!(state.drain(Some(session.id())).is_empty());
    let saved = side_thread::load(&path).unwrap();
    assert_eq!(
        saved.pending.as_ref().unwrap().answer,
        "durable side prefix"
    );
    drop(state);
    assert!(cancel.load(Ordering::SeqCst));

    let mut restored = SideThreadState::default();
    restored.bind(&session).unwrap();
    assert!(!restored.busy());
    let saved = side_thread::load(&path).unwrap();
    assert!(saved.pending.is_none());
    let exchange = &saved.thread("1").unwrap().exchanges[0];
    assert_eq!(exchange.outcome, Outcome::Interrupted);
    assert_eq!(exchange.question, "why?");
    assert_eq!(exchange.answer, "durable side prefix");
}

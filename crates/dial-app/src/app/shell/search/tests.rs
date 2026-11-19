use super::*;
use crate::storage::session::SearchHit;
use std::sync::Mutex;
use std::sync::mpsc::{SyncSender, sync_channel};

type Calls = Arc<Mutex<Vec<String>>>;

fn hit(id: &str) -> SearchHit {
    SearchHit {
        session_id: id.to_owned(),
        entry_id: "entry-1".into(),
        preview: "Full matched words".into(),
        role: "user".to_owned(),
        snippet: "matched words".to_owned(),
        when: String::new(),
    }
}

/// A runner that records each query and answers at once with one hit named after it.
fn recording(calls: Calls) -> Runner {
    Arc::new(move |_, query, _| {
        calls.lock().unwrap().push(query.to_owned());
        Ok(vec![hit(query)])
    })
}

/// A runner that records each query, then blocks until the test sends a token.
fn gated(calls: Calls) -> (SyncSender<()>, Runner) {
    let (gate_tx, gate_rx) = sync_channel::<()>(4);
    let gate_rx = Mutex::new(gate_rx);
    let runner: Runner = Arc::new(move |_, query, _| {
        calls.lock().unwrap().push(query.to_owned());
        gate_rx.lock().unwrap().recv().unwrap();
        Ok(vec![hit(query)])
    });
    (gate_tx, runner)
}

fn other_project(layout: &Layout) -> Layout {
    let mut other = layout.clone();
    other.state_dir.push("other-project");
    other
}

fn queries(outcomes: &[Outcome]) -> Vec<&str> {
    outcomes.iter().map(|o| o.query.as_str()).collect()
}

#[test]
fn only_the_latest_query_publishes_and_waiting_requests_coalesce() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let calls: Calls = Default::default();
    let mut search = Search::with_runner(recording(calls.clone()));

    search.request(&layout, "a", "first");
    search.request(&layout, "a", "second");
    search.request(&layout, "a", "third");
    let outcomes = search.settle(&layout, "a");

    assert_eq!(queries(&outcomes), ["third"]);
    // "first" was already running and "second" was replaced while waiting: two runs, not three.
    assert_eq!(*calls.lock().unwrap(), ["first", "third"]);
    assert!(!search.is_pending());
}

#[test]
fn request_returns_at_once_while_the_worker_is_still_running() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let (gate, runner) = gated(Default::default());
    let mut search = Search::with_runner(runner);

    search.request(&layout, "a", "slow");

    assert!(search.is_pending());
    assert!(
        search.poll(&layout, "a").is_empty(),
        "nothing publishes before the worker finishes"
    );
    gate.send(()).unwrap();
    assert_eq!(queries(&search.settle(&layout, "a")), ["slow"]);
}

#[test]
fn a_result_for_another_project_is_never_published() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let mut search = Search::with_runner(recording(Default::default()));

    search.request(&layout, "a", "parser");

    assert!(search.settle(&other_project(&layout), "a").is_empty());
    assert!(!search.is_pending());
}

#[test]
fn a_waiting_request_for_another_project_is_dropped_not_run() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let (gate, runner) = gated(Default::default());
    let mut search = Search::with_runner(runner);
    let other = other_project(&layout);

    search.request(&layout, "a", "here");
    search.request(&other, "a", "elsewhere");
    gate.send(()).unwrap();
    let outcomes = search.settle(&layout, "a");

    assert!(
        outcomes.is_empty(),
        "the running and queued requests are stale"
    );
    assert!(!search.is_pending());
}

#[test]
fn a_result_is_dropped_after_a_session_change_in_the_same_project() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let (gate, runner) = gated(Default::default());
    let mut search = Search::with_runner(runner);

    search.request(&layout, "session-a", "parser");
    // `/new` or `/resume` changes the session while the worker still runs in the same project.
    gate.send(()).unwrap();

    assert!(search.settle(&layout, "session-b").is_empty());
    assert!(!search.is_pending());
}

#[test]
fn switching_away_and_back_before_a_poll_still_invalidates_the_request() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let (gate, runner) = gated(Default::default());
    let mut search = Search::with_runner(runner);

    search.request(&layout, "session-a", "parser");
    // Session A -> B -> A with no poll in between: the context reads the same at the next poll.
    search.invalidate();
    search.invalidate();
    gate.send(()).unwrap();

    assert!(search.settle(&layout, "session-a").is_empty());
}

#[test]
fn a_queued_request_made_before_a_switch_back_is_dropped_not_run() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let calls: Calls = Default::default();
    let (gate, runner) = gated(calls.clone());
    let mut search = Search::with_runner(runner);

    search.request(&layout, "session-a", "running");
    search.request(&layout, "session-a", "waiting");
    search.invalidate();
    search.invalidate();
    gate.send(()).unwrap();
    let outcomes = search.settle(&layout, "session-a");

    assert!(outcomes.is_empty());
    assert_eq!(
        *calls.lock().unwrap(),
        ["running"],
        "a stale queued request never starts"
    );
}

#[test]
fn a_request_made_after_the_change_publishes_in_the_new_session() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let mut search = Search::with_runner(recording(Default::default()));

    search.request(&layout, "session-a", "old");
    search.request(&layout, "session-b", "new");

    assert_eq!(queries(&search.settle(&layout, "session-b")), ["new"]);
}

#[test]
fn a_disconnected_worker_is_reported_as_an_error_not_silence() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let runner: Runner =
        Arc::new(|_, _, _| -> anyhow::Result<Vec<SearchHit>> { panic!("simulated worker crash") });
    let mut search = Search::with_runner(runner);

    search.request(&layout, "a", "boom");
    let outcomes = search.settle(&layout, "a");

    assert_eq!(outcomes.len(), 1);
    let error = outcomes[0].result.as_ref().unwrap_err().to_string();
    assert!(error.contains("stopped unexpectedly"), "{error}");
}

#[test]
fn shell_session_changes_invalidate_a_pending_search_even_when_switched_back() {
    let (_home, _project, app, mut shell) = crate::app::shell::testkit::shell();
    let (gate, runner) = gated(Default::default());
    shell.search = Search::with_runner(runner);
    shell.set_session_id("session-a");

    shell.start_search(&app.layout, "parser");
    shell.set_session_id("session-b");
    shell.set_session_id("session-a");
    gate.send(()).unwrap();

    assert!(shell.search.settle(&app.layout, "session-a").is_empty());
}

#[test]
fn search_text_keeps_the_transcript_wording_and_surfaces_errors() {
    let cases: [(anyhow::Result<Vec<SearchHit>>, &str); 3] = [
        (Ok(Vec::new()), "No past messages match \"parser\"."),
        (
            Ok(vec![hit("abcdef123456")]),
            "abcdef12 [user] matched words",
        ),
        (Err(anyhow::anyhow!("index locked")), "error: index locked"),
    ];
    for (result, expected) in cases {
        assert_eq!(search_text("parser", &result), expected);
    }
}

#[test]
fn ready_results_are_discarded_by_new_requests_and_session_changes() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let mut search = Search::with_runner(recording(Default::default()));
    search.ready = Some(("old".into(), page(vec![hit("old")])));
    search.request(&layout, "session-a", "new");
    assert!(search.ready.is_none());
    search.ready = Some(("new".into(), page(vec![hit("new")])));
    search.invalidate();
    assert!(search.ready.is_none());
    search.settle(&layout, "session-a");
}

#[test]
fn opening_multiple_results_keeps_first_origin_until_return() {
    let mut search = Search::default();
    search.remember_origin("original");
    search.remember_origin("another-hit");
    assert_eq!(search.origin(), Some("original"));
    search.invalidate();
    assert_eq!(search.origin(), Some("original"));
    search.clear_origin();
    assert_eq!(search.origin(), None);
}

#[test]
fn returning_to_origin_is_scoped_to_the_project() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let mut search = Search::default();
    search.sync(&layout, "original");
    search.remember_origin("original");
    search.sync(&other_project(&layout), "elsewhere");
    assert_eq!(search.origin(), None);
}

fn page(hits: Vec<SearchHit>) -> Page {
    Page {
        hits: hits.into(),
        titles: Default::default(),
        has_more: false,
        limit: SEARCH_LIMIT,
    }
}

#[test]
fn expanding_window_uses_lookahead_and_preserves_context_invalidation() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let runner: Runner =
        Arc::new(|_, _, limit| Ok((0..limit.min(75)).map(|i| hit(&i.to_string())).collect()));
    let mut search = Search::with_runner(runner);
    search.request(&layout, "session", "needle");
    let first = search
        .settle(&layout, "session")
        .pop()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(first.hits.len(), 50);
    assert!(first.has_more);
    search.request_window(&layout, "session", "needle", 100);
    let next = search
        .settle(&layout, "session")
        .pop()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(next.hits.len(), 75);
    assert!(!next.has_more);
    search.request_window(&layout, "session", "needle", 150);
    search.invalidate();
    assert!(search.settle(&layout, "session").is_empty());
    assert!(search.ready.is_none());
}

#[test]
fn worker_hydrates_titles_before_publishing_results() {
    let (_home, _project, layout, _) = crate::app::tests::config();
    let mut session = Session::create(
        &layout,
        crate::storage::session::SessionMeta {
            model: "fixture".into(),
        },
    )
    .unwrap();
    session
        .append(serde_json::json!({"role":"user", "content":"A distinctive title"}))
        .unwrap();
    let id = session.id().to_owned();
    let expected = Session::find_prefix(&layout, &id)
        .unwrap()
        .pop()
        .unwrap()
        .title;
    assert!(!expected.is_empty());
    let hit_id = id.clone();
    let mut search = Search::with_runner(Arc::new(move |_, _, _| {
        Ok(vec![hit(&hit_id), hit(&hit_id)])
    }));
    search.request(&layout, "active", "needle");
    let page = search
        .settle(&layout, "active")
        .pop()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(page.titles.len(), 1);
    assert_eq!(page.titles.get(&id), Some(&expected));
}

fn await_publication(shell: &mut Shell, layout: &Layout) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !shell.poll_search(layout) {
        assert!(
            std::time::Instant::now() < deadline,
            "search never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn inspected_results_remain_available_without_rerunning_the_query() {
    let (_home, _project, app, mut shell) = crate::app::shell::testkit::shell();
    let calls: Calls = Default::default();
    shell.search = Search::with_runner(recording(calls.clone()));
    shell.start_search(&app.layout, "parser");
    await_publication(&mut shell, &app.layout);
    assert!(shell.search_panel(&app.layout).is_some());
    assert!(
        shell.search_panel(&app.layout).is_some(),
        "reopen the same hits"
    );
    shell.more_search(&app.layout);
    assert!(!shell.search.is_pending(), "all hits were already loaded");
    assert_eq!(*calls.lock().unwrap(), ["parser"]);
    assert!(shell.search_panel(&other_project(&app.layout)).is_none());
}

#[test]
fn failed_expansion_retains_the_page_and_retries_the_same_window() {
    let (_home, _project, app, mut shell) = crate::app::shell::testkit::shell();
    let (tx, rx) = sync_channel(1);
    let replies = Mutex::new(rx);
    let limits = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&limits);
    shell.search = Search::with_runner(Arc::new(move |_, _, limit| {
        observed.lock().unwrap().push(limit);
        replies.lock().unwrap().recv().unwrap()
    }));
    shell.start_search(&app.layout, "parser");
    tx.send(Ok(vec![hit("same"); SEARCH_LIMIT + 1])).unwrap();
    await_publication(&mut shell, &app.layout);
    shell.more_search(&app.layout);
    shell.more_search(&app.layout);
    assert!(
        shell.search_panel(&app.layout).is_some(),
        "read while loading"
    );
    tx.send(Err(anyhow!("index unavailable"))).unwrap();
    await_publication(&mut shell, &app.layout);
    assert!(
        shell.search_panel(&app.layout).is_some(),
        "read after failure"
    );
    shell.more_search(&app.layout);
    tx.send(Ok(vec![hit("same"); 72])).unwrap();
    await_publication(&mut shell, &app.layout);
    assert_eq!(shell.search.ready.as_ref().unwrap().1.hits.len(), 72);
    assert_eq!(*limits.lock().unwrap(), [51, 101, 101]);
}

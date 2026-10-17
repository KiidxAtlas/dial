use super::*;
use std::io::Read;
use std::sync::atomic::AtomicUsize;

#[test]
fn test_idle_limits_are_isolated_from_concurrent_request_threads() {
    let (ready, started) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            Patience::with_test_idle_limit(Duration::from_millis(37));
            ready.send(Patience::for_stream(1, false).idle).unwrap();
            wait.recv().unwrap();
        });
        let overridden = started.recv().unwrap();
        let unaffected = Patience::for_stream(1, false).idle;
        release.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(overridden, Duration::from_millis(37));
        assert_eq!(unaffected, Duration::from_secs(1));
    });
    assert_eq!(Patience::for_stream(1, false).idle, Duration::from_secs(1));
}

#[test]
fn test_setup_budget_is_generous_while_idle_after_progress_stays_tight() {
    Patience::with_test_idle_limit(Duration::from_millis(37));
    let patience = Patience::for_stream(1, false);
    assert_eq!(patience.idle, Duration::from_millis(37));
    assert_eq!(patience.first_event, Some(TEST_SETUP_BUDGET));
    patience.mark_event();
    assert!(patience.remaining().unwrap() <= Duration::from_millis(37));
}

#[test]
fn test_idle_limit_scope_is_restored_after_the_call() {
    assert_eq!(Patience::for_stream(1, false).idle, Duration::from_secs(1));
    {
        let _scope = TestIdleLimitScope::enter(Some(Duration::from_millis(41)));
        assert_eq!(
            Patience::for_stream(1, false).idle,
            Duration::from_millis(41)
        );
    }
    assert_eq!(Patience::for_stream(1, false).idle, Duration::from_secs(1));
}

#[derive(Debug)]
struct RecordingResolver {
    calls: Arc<AtomicUsize>,
}
impl Resolver for RecordingResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        DefaultResolver::default().resolve(uri, config, timeout)
    }
}

#[test]
fn custom_guarded_resolver_is_preserved_and_silent_body_can_be_cancelled() {
    let server = super::super::fake::serve(vec![super::super::fake::Script::Stall {
        silence: Duration::from_millis(400),
    }]);
    let interrupt = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(3)))
        .build();
    let agent = agent_with_resolver(
        config,
        interrupt.clone(),
        RecordingResolver {
            calls: calls.clone(),
        },
    );
    let mut response = agent.get(&server.url).call().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        interrupt.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let error = response
        .body_mut()
        .as_reader()
        .read_to_end(&mut Vec::new())
        .unwrap_err();
    trigger.join().unwrap();
    assert!(error.to_string().contains("interrupted"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn pre_cancelled_requests_do_not_invoke_custom_dns() {
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = agent_with_resolver(
        ureq::Agent::config_builder().proxy(None).build(),
        Arc::new(AtomicBool::new(true)),
        RecordingResolver {
            calls: calls.clone(),
        },
    );
    assert!(agent.get("http://127.0.0.1:9").call().is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn zero_patience_has_no_deadline_before_or_after_progress() {
    let patience = Patience::for_stream(0, false);
    assert!(patience.remaining().is_none());
    patience.mark_event();
    assert!(patience.remaining().is_none());
    assert!(Patience::for_stream(1, false).remaining().is_some());
}

#[test]
fn remote_default_bounds_first_event_and_silence_unless_local_or_opted_out() {
    let limit = Duration::from_secs(crate::config::types::DEFAULT_REMOTE_IDLE_TIMEOUT_SECS);
    let remote = Patience::for_stream(
        crate::config::types::DEFAULT_REMOTE_IDLE_TIMEOUT_SECS,
        false,
    );
    assert_eq!(remote.first_event, Some(limit));
    assert_eq!(remote.idle, limit);
    assert!(
        Patience::for_stream(crate::config::types::DEFAULT_REMOTE_IDLE_TIMEOUT_SECS, true)
            .first_event
            .is_none()
    );
    assert!(Patience::for_stream(0, false).first_event.is_none());
}

#[test]
fn idle_deadline_survives_a_read_boundary() {
    let server = super::super::fake::serve(vec![super::super::fake::Script::Stall {
        silence: Duration::from_secs(2),
    }]);
    let patience = Patience::new(Some(Duration::from_secs(1)), Duration::from_millis(240));
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .http_status_as_error(false)
        .timeout_recv_response(Some(Duration::from_secs(5)))
        .timeout_recv_body(Some(Duration::from_secs(5)))
        .build();
    let agent = patient_agent(config, Arc::new(AtomicBool::new(false)), patience.clone());
    let mut response = agent.get(&server.url).call().unwrap();
    patience.mark_event();
    // Keep-alive bytes, parsing and UI work between reads must not restart the event deadline.
    std::thread::sleep(Duration::from_millis(180));
    let started = Instant::now();
    let error = response
        .body_mut()
        .as_reader()
        .read_to_end(&mut Vec::new())
        .unwrap_err();
    assert!(
        error.to_string().to_ascii_lowercase().contains("timeout"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(160),
        "idle budget restarted at the read: {:?}",
        started.elapsed()
    );
}

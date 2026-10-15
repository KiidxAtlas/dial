//! Cancellation is polled below TLS, without turning healthy silence into a timeout.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use ureq::unversioned::{
    resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
    transport::{
        Buffers, ConnectProxyConnector, ConnectionDetails, Connector, NextTimeout, RustlsConnector,
        TcpConnector, Transport,
    },
};

// Test seam: scale one-second stall-test limits down without leaking into concurrent tests.
// Each test drives its model call on its own thread; zero preserves production limits. Backends
// carry their own limit (see `TestIdleLimitScope`) for calls that run on a worker thread.
#[cfg(test)]
thread_local! {
    static TEST_IDLE_LIMIT_MILLIS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Pre-progress budget under the test seam: connect, request write, response head, and the wait for the
/// first meaningful event. Generous on purpose: a loaded machine can delay the scripted server's head past
/// the 200 ms idle limit, which then fails as a setup timeout rather than a stalled body. Silence after
/// progress keeps the tight test idle limit, so body stalls are still measured tightly.
#[cfg(test)]
const TEST_SETUP_BUDGET: Duration = Duration::from_millis(1500);

#[cfg(test)]
fn test_limit_active(idle_secs: u64) -> bool {
    idle_secs == 1 && TEST_IDLE_LIMIT_MILLIS.with(std::cell::Cell::get) != 0
}

fn stream_idle_limit(idle_secs: u64) -> Duration {
    #[cfg(test)]
    {
        if test_limit_active(idle_secs) {
            return Duration::from_millis(TEST_IDLE_LIMIT_MILLIS.with(std::cell::Cell::get));
        }
    }
    Duration::from_secs(idle_secs)
}

fn first_event_limit(_idle_secs: u64, idle: Duration) -> Duration {
    #[cfg(test)]
    {
        if test_limit_active(_idle_secs) {
            return TEST_SETUP_BUDGET;
        }
    }
    idle
}

/// How long a streaming reply may stay silent. A local model can spend minutes reading a long prompt before
/// its first token, then streams steadily, so one cap on the whole call either kills healthy cold starts or
/// never catches a real stall. One value per request: `seen` is shared between the protocol parser, which sets it on
/// meaningful progress, and the transport, which applies the matching limit to each blocking read.
#[derive(Clone, Debug)]
pub(crate) struct Patience {
    /// Longest wait for the first event. `None` waits as long as the request is not cancelled: the right
    /// setting for a model server on this machine or LAN.
    pub first_event: Option<Duration>,
    /// Longest silence once the reply has started.
    pub idle: Duration,
    pub(super) seen: Arc<AtomicBool>,
    clock: Arc<parking_lot::Mutex<ProgressClock>>,
}

#[derive(Debug)]
struct ProgressClock {
    last_event: Instant,
    terminal_deadline: Option<Instant>,
}

impl Patience {
    pub(crate) fn new(first_event: Option<Duration>, idle: Duration) -> Self {
        Patience {
            first_event,
            idle,
            seen: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(parking_lot::Mutex::new(ProgressClock {
                last_event: Instant::now(),
                terminal_deadline: None,
            })),
        }
    }

    pub(crate) fn for_stream(idle_secs: u64, local: bool) -> Self {
        let idle = stream_idle_limit(idle_secs);
        Self::new(
            if local || idle_secs == 0 {
                None
            } else {
                Some(first_event_limit(idle_secs, idle))
            },
            idle,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_test_idle_limit(limit: Duration) {
        TEST_IDLE_LIMIT_MILLIS.with(|millis| millis.set(limit.as_millis() as u64));
    }

    pub(super) fn first_event_seen(&self) -> bool {
        self.seen.load(Ordering::SeqCst)
    }
    pub(super) fn mark_event(&self) {
        self.clock.lock().last_event = Instant::now();
        self.seen.store(true, Ordering::SeqCst);
    }

    /// Bound draining optional trailing usage after the provider commits a reply.
    /// Later progress must never extend this terminal grace window.
    pub(super) fn finish_within(&self, grace: Duration) {
        self.clock
            .lock()
            .terminal_deadline
            .get_or_insert_with(|| Instant::now() + grace);
    }

    pub(super) fn remaining(&self) -> Option<Duration> {
        let clock = self.clock.lock();
        let limit = if self.first_event_seen() {
            (!self.idle.is_zero()).then_some(self.idle)
        } else {
            self.first_event
        };
        let remaining = limit.map(|limit| limit.saturating_sub(clock.last_event.elapsed()));
        match (remaining, clock.terminal_deadline) {
            (Some(remaining), Some(deadline)) => {
                Some(remaining.min(deadline.saturating_duration_since(Instant::now())))
            }
            (None, Some(deadline)) => Some(deadline.saturating_duration_since(Instant::now())),
            (remaining, None) => remaining,
        }
    }
}
/// Test-only: the thread-local idle limit set for one model call, restored when the call returns. A backend
/// carries its limit to the worker thread that runs the call; no other thread observes it.
#[cfg(test)]
pub(crate) struct TestIdleLimitScope {
    previous: u64,
}

#[cfg(test)]
impl TestIdleLimitScope {
    pub(crate) fn enter(limit: Option<Duration>) -> Self {
        let previous = TEST_IDLE_LIMIT_MILLIS.with(std::cell::Cell::get);
        if let Some(limit) = limit {
            TEST_IDLE_LIMIT_MILLIS.with(|millis| millis.set(limit.as_millis() as u64));
        }
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for TestIdleLimitScope {
    fn drop(&mut self) {
        TEST_IDLE_LIMIT_MILLIS.with(|millis| millis.set(self.previous));
    }
}

pub(super) fn agent(config: ureq::config::Config, interrupt: Arc<AtomicBool>) -> ureq::Agent {
    agent_with_resolver(config, interrupt, DefaultResolver::default())
}

pub(super) fn agent_with_resolver(
    config: ureq::config::Config,
    interrupt: Arc<AtomicBool>,
    resolver: impl Resolver,
) -> ureq::Agent {
    build(config, interrupt, resolver, None)
}

/// A streaming-chat agent: silence is bounded by `patience`, the stream as a whole is not.
pub(super) fn patient_agent(
    config: ureq::config::Config,
    interrupt: Arc<AtomicBool>,
    patience: Patience,
) -> ureq::Agent {
    build(
        config,
        interrupt,
        DefaultResolver::default(),
        Some(patience),
    )
}

fn build(
    config: ureq::config::Config,
    interrupt: Arc<AtomicBool>,
    resolver: impl Resolver,
    patience: Option<Patience>,
) -> ureq::Agent {
    let connector = ()
        .chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(CancelConnector {
            interrupt: Arc::clone(&interrupt),
            patience,
        })
        .chain(RustlsConnector::default());
    ureq::Agent::with_parts(
        config,
        connector,
        CancelResolver {
            inner: resolver,
            interrupt,
        },
    )
}

#[derive(Debug)]
struct CancelResolver<R> {
    inner: R,
    interrupt: Arc<AtomicBool>,
}
impl<R: Resolver> Resolver for CancelResolver<R> {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        check(&self.interrupt)?;
        let result = self.inner.resolve(uri, config, timeout);
        check(&self.interrupt)?;
        result
    }
}

fn check(interrupt: &AtomicBool) -> Result<(), ureq::Error> {
    if interrupt.load(Ordering::SeqCst) {
        Err(ureq::Error::Other(Box::new(super::Interrupted)))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
struct CancelConnector {
    interrupt: Arc<AtomicBool>,
    patience: Option<Patience>,
}
impl<T: Transport> Connector<T> for CancelConnector {
    type Out = CancelTransport<T>;
    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<T>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        check(&self.interrupt)?;
        Ok(chained.map(|inner| CancelTransport {
            inner,
            interrupt: Arc::clone(&self.interrupt),
            patience: self.patience.clone(),
        }))
    }
}

#[derive(Debug)]
struct CancelTransport<T> {
    inner: T,
    interrupt: Arc<AtomicBool>,
    patience: Option<Patience>,
}
impl<T: Transport> CancelTransport<T> {
    fn check(&self) -> Result<(), ureq::Error> {
        check(&self.interrupt)
    }
}
impl<T: Transport> Transport for CancelTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.check()?;
        // A partial write must never be replayed; retain the configured write deadline.
        self.inner.transmit_output(amount, timeout)
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let started = Instant::now();
        // The SSE reader owns the progress clock. A read boundary or keep-alive bytes must never
        // reset it; otherwise a final keep-alive followed by silence grants another full idle budget.
        let patience = self.patience.as_ref().filter(|_| {
            matches!(
                timeout.reason,
                ureq::Timeout::RecvResponse | ureq::Timeout::RecvBody
            )
        });
        loop {
            self.check()?;
            let mut remaining = timeout.after.saturating_sub(started.elapsed());
            if let Some(limit) = patience.and_then(Patience::remaining) {
                remaining = remaining.min(limit);
            }
            if remaining.is_zero() {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            let slice = NextTimeout {
                after: remaining.min(Duration::from_millis(100)).into(),
                reason: timeout.reason,
            };
            match self.inner.await_input(slice) {
                Err(ureq::Error::Timeout(_)) => continue,
                result => return result,
            }
        }
    }
    fn is_open(&mut self) -> bool {
        !self.interrupt.load(Ordering::SeqCst) && self.inner.is_open()
    }
    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests {
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
}

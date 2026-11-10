//! Cancellation is polled below TLS, without turning healthy silence into a timeout.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use ureq::unversioned::{
    resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
    transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport},
};

mod reuse;
mod tcp;
pub(super) use tcp::keepalive;

/// What the request a connection currently serves may wait for. Each connection owns one cell,
/// shared by its cancellable layers; the reuse pool rebinds it when it lends the connection to the
/// next request, so cancellation and silence limits always belong to the request in flight.
#[derive(Clone, Debug)]
struct Binding {
    interrupt: Arc<AtomicBool>,
    patience: Option<Patience>,
}
type Bound = Arc<parking_lot::Mutex<Binding>>;

// Test seam: scale one-second stall-test limits down without leaking into concurrent tests.
// Each test drives its model call on its own thread; zero preserves production limits. Backends
// carry their own limit (see `TestIdleLimitScope`) for calls that run on a worker thread.
#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_IDLE_LIMIT_MILLIS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

// Captured by each Patience, so a controlled stream can cross idle deadlines without wall-clock
// sleeps. Install only on a dedicated test thread; other requests retain the real clock.
#[cfg(test)]
thread_local! {
    static TEST_CLOCK: std::cell::RefCell<Option<TestClock>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct TestClock {
    now: Arc<parking_lot::Mutex<Instant>>,
    progress: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
impl TestClock {
    pub(crate) fn new() -> (Self, std::sync::mpsc::Receiver<()>) {
        let (progress, receiver) = std::sync::mpsc::channel();
        (
            Self {
                now: Arc::new(parking_lot::Mutex::new(Instant::now())),
                progress,
            },
            receiver,
        )
    }

    pub(crate) fn advance(&self, duration: Duration) {
        *self.now.lock() += duration;
    }

    pub(crate) fn install(self) {
        TEST_CLOCK.with(|clock| *clock.borrow_mut() = Some(self));
    }
}

/// Pre-progress budget under the test seam: connect, request write, response head, and the wait for the
/// first meaningful event. Generous on purpose: a loaded machine can delay the scripted server's head past
/// the 200 ms idle limit, which then fails as a setup timeout rather than a stalled body. Silence after
/// progress keeps the tight test idle limit, so body stalls are still measured tightly.
#[cfg(any(test, feature = "test-support"))]
const TEST_SETUP_BUDGET: Duration = Duration::from_millis(1500);

#[cfg(any(test, feature = "test-support"))]
fn test_limit_active(idle_secs: u64) -> bool {
    idle_secs == 1 && TEST_IDLE_LIMIT_MILLIS.with(std::cell::Cell::get) != 0
}

fn stream_idle_limit(idle_secs: u64) -> Duration {
    #[cfg(any(test, feature = "test-support"))]
    {
        if test_limit_active(idle_secs) {
            return Duration::from_millis(TEST_IDLE_LIMIT_MILLIS.with(std::cell::Cell::get));
        }
    }
    Duration::from_secs(idle_secs)
}

fn first_event_limit(_idle_secs: u64, idle: Duration) -> Duration {
    #[cfg(any(test, feature = "test-support"))]
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
pub struct Patience {
    /// Longest wait for the first event. `None` waits as long as the request is not cancelled: the right
    /// setting for a model server on this machine or LAN.
    pub first_event: Option<Duration>,
    /// Longest silence once the reply has started.
    pub idle: Duration,
    pub(super) seen: Arc<AtomicBool>,
    clock: Arc<parking_lot::Mutex<ProgressClock>>,
    #[cfg(test)]
    test_clock: Option<TestClock>,
}

#[derive(Debug)]
struct ProgressClock {
    last_event: Instant,
    terminal_deadline: Option<Instant>,
}

impl Patience {
    pub(crate) fn new(first_event: Option<Duration>, idle: Duration) -> Self {
        #[cfg(test)]
        let test_clock = TEST_CLOCK.with(|clock| clock.borrow().clone());
        #[cfg(test)]
        let now = test_clock
            .as_ref()
            .map_or_else(Instant::now, |clock| *clock.now.lock());
        #[cfg(not(test))]
        let now = Instant::now();
        Patience {
            first_event,
            idle,
            seen: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(parking_lot::Mutex::new(ProgressClock {
                last_event: now,
                terminal_deadline: None,
            })),
            #[cfg(test)]
            test_clock,
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

    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(clock) = &self.test_clock {
            return *clock.now.lock();
        }
        Instant::now()
    }

    pub(super) fn first_event_seen(&self) -> bool {
        self.seen.load(Ordering::SeqCst)
    }
    pub(super) fn mark_event(&self) {
        self.clock.lock().last_event = self.now();
        self.seen.store(true, Ordering::SeqCst);
        #[cfg(test)]
        if let Some(clock) = &self.test_clock {
            let _ = clock.progress.send(());
        }
    }

    /// Bound draining optional trailing usage after the provider commits a reply.
    /// A later call may shorten this terminal grace window but never extend it.
    pub(super) fn finish_within(&self, grace: Duration) {
        let deadline = self.now() + grace;
        let mut clock = self.clock.lock();
        clock.terminal_deadline = Some(
            clock
                .terminal_deadline
                .map_or(deadline, |existing| existing.min(deadline)),
        );
    }

    pub(super) fn remaining(&self) -> Option<Duration> {
        let clock = self.clock.lock();
        let limit = if self.first_event_seen() {
            (!self.idle.is_zero()).then_some(self.idle)
        } else {
            self.first_event
        };
        let remaining =
            limit.map(|limit| limit.saturating_sub(self.now().duration_since(clock.last_event)));
        match (remaining, clock.terminal_deadline) {
            (Some(remaining), Some(deadline)) => {
                Some(remaining.min(deadline.saturating_duration_since(self.now())))
            }
            (None, Some(deadline)) => Some(deadline.saturating_duration_since(self.now())),
            (remaining, None) => remaining,
        }
    }

    /// Time left before the terminal deadline, if a committed reply set one. Unlike the idle and
    /// first-event limits, it bounds every read: ureq reports no response phase when no `recv_*`
    /// timeout is configured (an unlimited `timeout_secs`), and a committed reply must still end.
    fn terminal_remaining(&self) -> Option<Duration> {
        self.clock
            .lock()
            .terminal_deadline
            .map(|deadline| deadline.saturating_duration_since(self.now()))
    }
}
/// Test-only: the thread-local idle limit set for one model call, restored when the call returns. A backend
/// carries its limit to the worker thread that runs the call; no other thread observes it.
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct TestIdleLimitScope {
    previous: u64,
}

#[cfg(any(test, feature = "test-support"))]
impl TestIdleLimitScope {
    pub(crate) fn enter(limit: Option<Duration>) -> Self {
        let previous = TEST_IDLE_LIMIT_MILLIS.with(std::cell::Cell::get);
        if let Some(limit) = limit {
            TEST_IDLE_LIMIT_MILLIS.with(|millis| millis.set(limit.as_millis() as u64));
        }
        Self { previous }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestIdleLimitScope {
    fn drop(&mut self) {
        TEST_IDLE_LIMIT_MILLIS.with(|millis| millis.set(self.previous));
    }
}

pub(super) fn agent(config: ureq::config::Config, interrupt: Arc<AtomicBool>) -> ureq::Agent {
    build(config, interrupt, DefaultResolver::default(), None, true)
}

/// A caller's resolver may guard which addresses are reachable, so these agents never borrow a
/// pooled connection that another resolver approved.
pub(super) fn agent_with_resolver(
    config: ureq::config::Config,
    interrupt: Arc<AtomicBool>,
    resolver: impl Resolver,
) -> ureq::Agent {
    build(config, interrupt, resolver, None, false)
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
        true,
    )
}

/// `reuse` lets the agent share warm connections with other agents (see [`reuse`]); a proxied
/// agent never does, because a CONNECT tunnel must not return as a plain connection.
fn build(
    config: ureq::config::Config,
    interrupt: Arc<AtomicBool>,
    resolver: impl Resolver,
    patience: Option<Patience>,
    reuse: bool,
) -> ureq::Agent {
    let reuse = reuse && config.proxy().is_none();
    let connector = reuse::ReuseConnector::new(
        Binding {
            interrupt: Arc::clone(&interrupt),
            patience,
        },
        reuse,
    );
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
    bound: Bound,
}
impl<T: Transport> Connector<T> for CancelConnector {
    type Out = CancelTransport<T>;
    fn connect(
        &self,
        _: &ConnectionDetails,
        chained: Option<T>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        check(&self.bound.lock().interrupt)?;
        Ok(chained.map(|inner| CancelTransport {
            inner,
            bound: Arc::clone(&self.bound),
            send_closed: false,
            send_failure: None,
        }))
    }
}

#[derive(Debug)]
struct CancelTransport<T> {
    inner: T,
    bound: Bound,
    /// The peer closed the connection while the request was being sent (see [`early_close`]);
    /// the connection is never reused afterwards.
    send_closed: bool,
    /// Why the send stopped, reported if the peer closed without answering.
    send_failure: Option<std::io::Error>,
}
impl<T: Transport> CancelTransport<T> {
    fn check(&self) -> Result<(), ureq::Error> {
        check(&self.bound.lock().interrupt)
    }
}

/// A server may answer before reading the whole request (a rate limit, an oversized or invalid
/// body) and then close, which resets the connection under the remaining write. That answer is the
/// real outcome, so the send stops and the response is read; a close with no answer still fails
/// with the original send error. The phase is not checked: ureq may name the overall deadline
/// instead of the send phase, and a close during the TLS handshake ends the same way, because
/// the handshake's next read finds no answer and reports the send error.
fn early_close(io: &std::io::Error) -> bool {
    matches!(
        io.kind(),
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
    )
}

impl<T: Transport> Transport for CancelTransport<T> {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.check()?;
        if self.send_closed {
            // The peer stopped reading; the rest of the request is dropped, never replayed.
            return Ok(());
        }
        // A partial write must never be replayed; the TCP transport resumes at the sent offset and
        // checks the interrupt between short write slices.
        match self.inner.transmit_output(amount, timeout) {
            Err(ureq::Error::Io(io)) if early_close(&io) => {
                self.send_closed = true;
                self.send_failure = Some(io);
                Ok(())
            }
            result => result,
        }
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let started = Instant::now();
        let binding = self.bound.lock().clone();
        // The SSE reader owns the progress clock. A read boundary or keep-alive bytes must never
        // reset it; otherwise a final keep-alive followed by silence grants another full idle budget.
        let in_response = matches!(
            timeout.reason,
            ureq::Timeout::RecvResponse | ureq::Timeout::RecvBody
        );
        let limit = |patience: &Patience| {
            if in_response {
                patience.remaining()
            } else {
                patience.terminal_remaining()
            }
        };
        loop {
            check(&binding.interrupt)?;
            let mut remaining = timeout.after.saturating_sub(started.elapsed());
            if let Some(limit) = binding.patience.as_ref().and_then(limit) {
                remaining = remaining.min(limit);
            }
            if remaining.is_zero() {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            let slice = NextTimeout {
                after: remaining.min(Duration::from_millis(100)).into(),
                reason: timeout.reason,
                per_read: timeout.per_read,
                per_write: timeout.per_write,
            };
            match self.inner.await_input(slice) {
                Err(ureq::Error::Timeout(_)) => continue,
                Ok(true) => {
                    // The peer answered; its response decides the outcome.
                    self.send_failure = None;
                    return Ok(true);
                }
                // Nothing arrived after the peer closed mid-send: report why the send stopped.
                result => {
                    return match self.send_failure.take() {
                        Some(io) => Err(io.into()),
                        None => result,
                    };
                }
            }
        }
    }
    fn is_open(&mut self) -> bool {
        !self.send_closed
            && !self.bound.lock().interrupt.load(Ordering::SeqCst)
            && self.inner.is_open()
    }
    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests;

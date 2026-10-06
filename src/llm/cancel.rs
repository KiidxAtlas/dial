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

/// How long a streaming reply may stay silent. A local model can spend minutes reading a long prompt before
/// its first token, then streams steadily, so one cap on the whole call either kills healthy cold starts or
/// never catches a real stall. One value per request: `seen` is shared between the SSE reader, which sets it on
/// the first `data:` event, and the transport, which applies the matching limit to each blocking read.
#[derive(Clone, Debug)]
pub(crate) struct Patience {
    /// Longest wait for the first event. `None` waits as long as the request is not cancelled: the right
    /// setting for a model server on this machine or LAN.
    pub first_event: Option<Duration>,
    /// Longest silence once the reply has started.
    pub idle: Duration,
    pub(super) seen: Arc<AtomicBool>,
    last_event: Arc<parking_lot::Mutex<Instant>>,
}

impl Patience {
    pub(crate) fn new(first_event: Option<Duration>, idle: Duration) -> Self {
        Patience {
            first_event,
            idle,
            seen: Arc::new(AtomicBool::new(false)),
            last_event: Arc::new(parking_lot::Mutex::new(Instant::now())),
        }
    }

    pub(crate) fn for_stream(idle_secs: u64, local: bool) -> Self {
        let idle = Duration::from_secs(idle_secs);
        Self::new(if local { None } else { Some(idle) }, idle)
    }

    pub(super) fn first_event_seen(&self) -> bool {
        self.seen.load(Ordering::SeqCst)
    }
    pub(super) fn mark_event(&self) {
        *self.last_event.lock() = Instant::now();
        self.seen.store(true, Ordering::SeqCst);
    }

    pub(super) fn remaining(&self) -> Option<Duration> {
        let last = self.last_event.lock();
        let limit = if self.first_event_seen() {
            Some(self.idle)
        } else {
            self.first_event
        };
        limit.map(|limit| limit.saturating_sub(last.elapsed()))
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

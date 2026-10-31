//! Warm connections shared across requests.
//!
//! Every request builds its own agent, because its interrupt and silence limits are its own, and an
//! agent's ureq pool dies with it. Without sharing, each round of a turn pays a fresh TCP and TLS
//! handshake to the model API, about a tenth of a second per request to a remote host. Here a
//! connection an agent finished with returns to one process-wide pool, and the next agent for the same
//! origin borrows it after rebinding it to its own request (see [`Binding`]).
//!
//! Only connections ureq itself would keep return: the response was read to its end with nothing
//! unread, the peer has not closed it, and the request was not interrupted. A borrowed connection is
//! probed again before use and expires after [`MAX_IDLE`]; TCP keepalive closes a path that died while
//! idle. Three kinds of connection are never shared:
//! - to loopback and LAN hosts: connecting costs nothing there, and local servers (model runners,
//!   test doubles) often answer one request per connection;
//! - through a proxy: a CONNECT tunnel must never come back as a plain connection;
//! - for an agent with a caller-supplied resolver, which may restrict the addresses it may reach.
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use ureq::unversioned::transport::{
    Buffers, ConnectProxyConnector, ConnectionDetails, Connector, NextTimeout, RustlsConnector,
    Transport,
};

use super::{Binding, Bound, CancelConnector, tcp::KeepaliveConnector};

/// Longest a connection waits in the pool. Below the idle timeouts model APIs and their front
/// proxies apply, so a borrowed connection is rarely one the server is about to close, yet long
/// enough to span the tool calls between two model requests of a turn.
const MAX_IDLE: Duration = Duration::from_secs(30);
/// Most idle connections kept per origin; concurrent subagents rarely need more.
const MAX_PER_ORIGIN: usize = 4;

static POOL: LazyLock<parking_lot::Mutex<HashMap<String, Vec<Idle>>>> =
    LazyLock::new(Default::default);

// Test seam: let loopback test servers stand in for a remote origin on this thread only.
#[cfg(test)]
thread_local! {
    static SHARE_LOOPBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct Idle {
    transport: Box<dyn Transport>,
    bound: Bound,
    since: Instant,
}

/// Opens connections through the cancellable TCP and TLS stack, borrowing a pooled one first when
/// the agent may share.
#[derive(Debug)]
pub(super) struct ReuseConnector {
    binding: Binding,
    share: bool,
    proxy: ConnectProxyConnector,
    tls: RustlsConnector,
}

impl ReuseConnector {
    pub(super) fn new(binding: Binding, share: bool) -> Self {
        ReuseConnector {
            binding,
            share,
            proxy: ConnectProxyConnector::default(),
            tls: RustlsConnector::default(),
        }
    }

    /// The pool key for this request, or `None` when it must use a connection of its own.
    fn origin(&self, details: &ConnectionDetails) -> Option<String> {
        if !self.share || details.request_level {
            return None;
        }
        let host = details.uri.host()?;
        #[cfg(test)]
        let loopback_shared = SHARE_LOOPBACK.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let loopback_shared = false;
        if crate::llm::is_local_host(host) && !loopback_shared {
            return None;
        }
        Some(format!(
            "{}://{}",
            details.uri.scheme_str()?,
            details.uri.authority()?
        ))
    }

    fn open(
        &self,
        details: &ConnectionDetails,
    ) -> Result<(Box<dyn Transport>, Bound), ureq::Error> {
        let bound: Bound = Arc::new(parking_lot::Mutex::new(self.binding.clone()));
        let proxied = Connector::<()>::connect(&self.proxy, details, None)?;
        let tcp = KeepaliveConnector {
            bound: Arc::clone(&bound),
        }
        .connect(details, proxied)?;
        let cancellable = CancelConnector {
            bound: Arc::clone(&bound),
        }
        .connect(details, tcp)?;
        let transport = self
            .tls
            .connect(details, cancellable)?
            .ok_or(ureq::Error::ConnectionFailed)?;
        Ok((Box::new(transport), bound))
    }
}

impl Connector<()> for ReuseConnector {
    type Out = Lent;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        super::check(&self.binding.interrupt)?;
        let origin = self.origin(details);
        if let Some(lent) = origin
            .as_ref()
            .and_then(|origin| borrow(origin, &self.binding))
        {
            return Ok(Some(lent));
        }
        let (transport, bound) = self.open(details)?;
        Ok(Some(Lent {
            transport: Some(transport),
            bound,
            origin,
            idle_since: None,
        }))
    }
}

/// The newest live pooled connection for `origin`, rebound to `binding`.
fn borrow(origin: &str, binding: &Binding) -> Option<Lent> {
    loop {
        let idle = {
            let mut pool = POOL.lock();
            let list = pool.get_mut(origin)?;
            let idle = list.pop();
            if list.is_empty() {
                pool.remove(origin);
            }
            idle?
        };
        if idle.since.elapsed() >= MAX_IDLE {
            continue;
        }
        *idle.bound.lock() = binding.clone();
        let mut transport = idle.transport;
        // Rebound first, so an interrupted request does not take it; a peer that closed while it
        // waited fails the probe, and the connection is dropped.
        if transport.is_open() {
            return Some(Lent {
                transport: Some(transport),
                bound: idle.bound,
                origin: Some(origin.to_owned()),
                idle_since: None,
            });
        }
    }
}

fn give_back(origin: String, idle: Idle) {
    let mut pool = POOL.lock();
    pool.retain(|_, list| {
        list.retain(|idle| idle.since.elapsed() < MAX_IDLE);
        !list.is_empty()
    });
    let list = pool.entry(origin).or_default();
    list.push(idle);
    if list.len() > MAX_PER_ORIGIN {
        list.remove(0);
    }
}

/// A connection serving one request at a time. It returns to the pool when dropped only if ureq's
/// last word on it was that it is open and idle: ureq checks for unread bytes and probes the socket
/// before it keeps a connection, and any later use clears the mark.
pub(super) struct Lent {
    transport: Option<Box<dyn Transport>>,
    bound: Bound,
    origin: Option<String>,
    idle_since: Option<Instant>,
}

impl Lent {
    fn inner(&mut self) -> &mut Box<dyn Transport> {
        self.transport
            .as_mut()
            .expect("a lent connection keeps its transport until dropped")
    }
}

impl std::fmt::Debug for Lent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lent")
            .field("transport", &self.transport)
            .field("origin", &self.origin)
            .finish()
    }
}

impl Transport for Lent {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner().buffers()
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.idle_since = None;
        self.inner().transmit_output(amount, timeout)
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.idle_since = None;
        self.inner().await_input(timeout)
    }
    fn is_open(&mut self) -> bool {
        let open = self.inner().is_open();
        self.idle_since = open.then(Instant::now);
        open
    }
    fn is_tls(&self) -> bool {
        self.transport.as_ref().is_some_and(|inner| inner.is_tls())
    }
}

impl Drop for Lent {
    fn drop(&mut self) {
        let (Some(since), Some(origin), Some(transport)) =
            (self.idle_since, self.origin.take(), self.transport.take())
        else {
            return;
        };
        give_back(
            origin,
            Idle {
                transport,
                bound: Arc::clone(&self.bound),
                since,
            },
        );
    }
}

#[cfg(test)]
mod tests;

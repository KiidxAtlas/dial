//! Plain TCP for model requests, with keepalive probes.
//!
//! A remote model can stay silent for minutes: before the response head, and between events while it
//! reasons. Without keepalive, a path that dies during that silence (sleep/wake, a Wi-Fi or VPN switch, a NAT
//! or firewall expiring an idle mapping) shows up only when the silence limit runs out, minutes later.
//! Probes keep idle mappings alive and turn a dead path into an error within about a minute. A live server
//! answers probes from its kernel, however long the model takes, so it keeps its full silence budget.
//!
//! Connect behaviour matches ureq's `TcpConnector`: resolved addresses share the connect budget, and an
//! address-specific failure (an unrouted IPv6 address, a refusal) moves on to the next address.
//!
//! Cancellation reaches both blocking phases a request starts with: the connect runs non-blocking
//! on the caller's own thread using `wait_socket_writable` in short slices (so an interrupt closes
//! the socket immediately without spawning or abandoning a helper thread), and the request body is
//! written in short slices that resume at the byte offset already sent, so an interrupt never waits
//! out the whole phase budget and a partially sent request is never replayed.
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, TcpKeepalive, Type};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport,
};

use super::Bound;

/// Idle time before the first probe.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
/// Time between unanswered probes.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// Unanswered probes before the kernel drops the connection.
const KEEPALIVE_PROBES: u32 = 3;
/// Smallest connect budget any one resolved address gets.
const MIN_PER_ADDRESS: Duration = Duration::from_millis(10);
/// How often a blocked connect or write looks at the interrupt flag.
const CANCEL_POLL: Duration = Duration::from_millis(100);
/// Most bytes handed to one `send`. Windows accepts a send of any size whenever its socket
/// buffer has room, so an unbounded send could absorb a whole request before the interrupt is
/// checked; bounded chunks keep cancellation responsive on every platform.
const MAX_WRITE: usize = 64 << 10;

/// Opens the TCP connection unless an earlier connector (a CONNECT proxy) already did.
#[derive(Debug)]
pub(super) struct KeepaliveConnector {
    pub(super) bound: Bound,
}

impl<In: Transport> Connector<In> for KeepaliveConnector {
    type Out = Either<In, KeepaliveTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        if chained.is_some() {
            return Ok(chained.map(Either::A));
        }
        let interrupt = Arc::clone(&self.bound.lock().interrupt);
        let stream = connect_cancellable(&details.addrs, details.timeout, &interrupt)?;
        keepalive(&stream)?;
        if details.config.no_delay() {
            stream.set_nodelay(true)?;
        }
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(Either::B(KeepaliveTransport {
            stream,
            buffers,
            bound: Arc::clone(&self.bound),
            read_timeout: None,
        })))
    }
}

/// Connects to the first reachable address without blocking past [`CANCEL_POLL`] on an interrupt
/// and without spawning a helper thread. Dropping the non-blocking socket on cancellation closes
/// the in-flight handshake immediately.
fn connect_cancellable(
    addrs: &[SocketAddr],
    timeout: NextTimeout,
    interrupt: &AtomicBool,
) -> Result<TcpStream, ureq::Error> {
    super::check(interrupt)?;
    let budget = timeout.not_zero().map(|after| *after);
    let started = Instant::now();
    let total_weight = 2.0 * (1.0 - 0.5_f64.powi(addrs.len() as i32));
    let mut weight = 1.0_f64;
    let mut last = None;
    for addr in addrs {
        super::check(interrupt)?;
        let per_addr = budget.map(|b| b.mul_f64(weight / total_weight).max(MIN_PER_ADDRESS));
        match connect_one_cancellable(*addr, per_addr, interrupt) {
            Ok(stream) => return Ok(stream),
            Err(ureq::Error::Io(error)) if address_specific(&error) => {
                last = Some(ureq::Error::Io(error));
            }
            Err(ureq::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                if budget.is_some_and(|budget| started.elapsed() > budget) {
                    return Err(ureq::Error::Timeout(timeout.reason));
                }
                last = Some(ureq::Error::Timeout(ureq::Timeout::Connect));
            }
            Err(error) => return Err(error),
        }
        weight /= 2.0;
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::ConnectionRefused, "Connection refused").into()
    }))
}

/// Non-blocking TCP connect to one address, polled in [`CANCEL_POLL`] slices so cancellation
/// stops the handshake immediately and closes the socket.
fn connect_one_cancellable(
    addr: SocketAddr,
    budget: Option<Duration>,
    interrupt: &AtomicBool,
) -> Result<TcpStream, ureq::Error> {
    super::check(interrupt)?;
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    let sock_addr = SockAddr::from(addr);
    match socket.connect(&sock_addr) {
        Ok(()) => {
            let stream: TcpStream = socket.into();
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
        Err(error) if is_in_progress(&error) => {}
        Err(error) => return Err(error.into()),
    }
    let stream: TcpStream = socket.into();
    let started = Instant::now();
    loop {
        super::check(interrupt)?;
        let mut slice = CANCEL_POLL;
        if let Some(budget) = budget {
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
            slice = slice.min(remaining);
        }
        let ready = crate::platform::io::wait_socket_writable(&stream, slice)?;
        if let Some(error) = SockRef::from(&stream).take_error()? {
            return Err(error.into());
        }
        if ready {
            // Verify the peer is actually connected; some kernels signal POLLHUP without SO_ERROR.
            if let Err(error) = stream.peer_addr() {
                return Err(error.into());
            }
            stream.set_nonblocking(false)?;
            return Ok(stream);
        }
    }
}

/// Whether `connect(2)` on a non-blocking socket returned the expected "handshake in progress" code.
fn is_in_progress(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EINPROGRESS) {
        return true;
    }
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(10035 | 10036)) {
        // WSAEWOULDBLOCK (10035) / WSAEINPROGRESS (10036)
        return true;
    }
    false
}

pub(in crate::llm) fn keepalive(stream: &TcpStream) -> io::Result<()> {
    SockRef::from(stream).set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_PROBES),
    )
}

/// Convenience wrapper for callers and tests without an external interrupt flag.
#[cfg(test)]
fn connect_any(addrs: &[SocketAddr], timeout: NextTimeout) -> Result<TcpStream, ureq::Error> {
    connect_cancellable(addrs, timeout, &AtomicBool::new(false))
}

/// This address refused or is unroutable from here; another resolved address may still work.
fn address_specific(error: &io::Error) -> bool {
    // A VPN or firewall on Windows can report a blocked address family as WSAEACCES.
    #[cfg(windows)]
    if error.raw_os_error() == Some(10013) {
        return true;
    }
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::AddrNotAvailable
    )
}

pub(super) struct KeepaliveTransport {
    stream: TcpStream,
    buffers: LazyBuffers,
    /// The request this connection serves; rebound when a pooled connection is lent again.
    bound: Bound,
    read_timeout: Option<Duration>,
}

impl std::fmt::Debug for KeepaliveTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeepaliveTransport")
            .field("addr", &self.stream.peer_addr().ok())
            .finish()
    }
}

impl KeepaliveTransport {
    /// The send loop of [`Transport::transmit_output`]; the stream must be non-blocking.
    fn send_nonblocking(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let budget = timeout.not_zero().map(|after| *after);
        let started = Instant::now();
        let interrupt = Arc::clone(&self.bound.lock().interrupt);
        let mut sent = 0;
        while sent < amount {
            super::check(&interrupt)?;
            let mut slice = CANCEL_POLL;
            if let Some(budget) = budget {
                let remaining = budget.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(ureq::Error::Timeout(timeout.reason));
                }
                slice = slice.min(remaining);
            }
            let output = &self.buffers.output()[sent..amount.min(sent + MAX_WRITE)];
            match self.stream.write(output) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(count) => sent += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // No room yet: wait one slice, then look at the interrupt and the phase budget.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    crate::platform::io::wait_socket_writable(&self.stream, slice)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// macOS refuses socket options (EINVAL) once the peer has reset the connection, yet what the
    /// peer sent before the reset (an early response) is still readable. Without a timeout a
    /// blocking read is unsafe, so this reads only what is already buffered. `None` when nothing
    /// is: the caller reports the original error.
    fn read_after_reset(&mut self) -> Option<usize> {
        self.stream.set_nonblocking(true).ok()?;
        let read = self.stream.read(self.buffers.input_append_buf());
        let _ = self.stream.set_nonblocking(false);
        read.ok()
    }
}

/// Set a socket timeout only when it changes; the patient transport re-arms reads every 100 ms.
fn arm(
    stream: &TcpStream,
    current: &mut Option<Duration>,
    timeout: NextTimeout,
    set: fn(&TcpStream, Option<Duration>) -> io::Result<()>,
) -> io::Result<()> {
    let wanted = timeout.not_zero().map(|after| *after);
    if wanted != *current {
        set(stream, wanted)?;
        *current = wanted;
    }
    Ok(())
}

/// An expired socket timeout is ureq's phase timeout, which the patient transport turns into another wait.
/// A connection the kernel dropped after unanswered probes is a dead connection and must surface as one.
/// Unix reports the first as `WouldBlock` (EAGAIN) and the second as `TimedOut` (ETIMEDOUT); Windows
/// reports an expired socket timeout as `TimedOut`, so there both stay timeouts.
fn io_failure(error: io::Error, timeout: NextTimeout) -> ureq::Error {
    if expired(&error) {
        ureq::Error::Timeout(timeout.reason)
    } else {
        error.into()
    }
}

/// Whether this error is an expired socket timeout (see [`io_failure`]).
fn expired(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock)
        || (cfg!(windows) && matches!(error.kind(), io::ErrorKind::TimedOut))
}

impl Transport for KeepaliveTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    /// Sends non-blocking chunks of at most [`MAX_WRITE`] bytes, waiting at most [`CANCEL_POLL`]
    /// at a time for room, so an interrupt is noticed while a slow peer holds the write. A
    /// non-blocking send accepts a definite count or nothing (`WouldBlock`), so resuming at the
    /// offset already sent never sends a byte twice. A socket send timeout is not used: on
    /// Windows an expired one leaves the socket state indeterminate.
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.set_nonblocking(true)?;
        let sent = self.send_nonblocking(amount, timeout);
        let restored = self.stream.set_nonblocking(false);
        sent?;
        Ok(restored?)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let armed = arm(
            &self.stream,
            &mut self.read_timeout,
            timeout,
            TcpStream::set_read_timeout,
        );
        let amount = match armed {
            Ok(()) => self
                .stream
                .read(self.buffers.input_append_buf())
                .map_err(|error| io_failure(error, timeout))?,
            Err(error) => self.read_after_reset().ok_or(error)?,
        };
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    /// Reusable only when no unread bytes or close are pending.
    fn is_open(&mut self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let idle = matches!(
            self.stream.read(&mut [0]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        );
        self.stream.set_nonblocking(false).is_ok() && idle
    }
}

#[cfg(test)]
mod tests;

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
//! Cancellation reaches both blocking phases a request starts with: the connect runs on a helper thread
//! the request stops waiting for, and the request body is written in short slices that resume at the
//! byte offset already sent, so an interrupt never waits out the whole phase budget and a partially sent
//! request is never replayed.
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{
    Arc,
    atomic::AtomicBool,
    mpsc::{self, RecvTimeoutError},
};
use std::time::{Duration, Instant};

use socket2::{SockRef, TcpKeepalive};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport,
};

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
/// Most bytes handed to one `write`. Windows accepts a send of any size whenever its socket
/// buffer has room, so an unbounded write could absorb a whole request before the interrupt is
/// checked; bounded chunks keep cancellation responsive on every platform.
const MAX_WRITE: usize = 64 << 10;

/// Opens the TCP connection unless an earlier connector (a CONNECT proxy) already did.
#[derive(Debug)]
pub(super) struct KeepaliveConnector {
    pub(super) interrupt: Arc<AtomicBool>,
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
        let stream = connect_cancellable(&details.addrs, details.timeout, &self.interrupt)?;
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
            interrupt: Arc::clone(&self.interrupt),
            read_timeout: None,
            write_timeout: None,
        })))
    }
}

/// [`connect_any`] on a helper thread, abandoned as soon as the request is interrupted. The abandoned
/// attempt ends on its own within its connect budget and drops whatever socket it opened.
fn connect_cancellable(
    addrs: &[SocketAddr],
    timeout: NextTimeout,
    interrupt: &AtomicBool,
) -> Result<TcpStream, ureq::Error> {
    super::check(interrupt)?;
    let addrs = addrs.to_vec();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("dial-connect".into())
        .spawn(move || {
            let _ = tx.send(connect_any(&addrs, timeout));
        })?;
    loop {
        match rx.recv_timeout(CANCEL_POLL) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => super::check(interrupt)?,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("the connect attempt ended without a result").into());
            }
        }
    }
}

fn keepalive(stream: &TcpStream) -> io::Result<()> {
    SockRef::from(stream).set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_PROBES),
    )
}

/// The first address that connects. Budgets follow a halving series that sums to the whole connect budget,
/// so a host with several addresses still finishes within it.
fn connect_any(addrs: &[SocketAddr], timeout: NextTimeout) -> Result<TcpStream, ureq::Error> {
    let budget = timeout.not_zero().map(|after| *after);
    let started = Instant::now();
    let total_weight = 2.0 * (1.0 - 0.5_f64.powi(addrs.len() as i32));
    let mut weight = 1.0_f64;
    let mut last = None;
    for addr in addrs {
        let attempt = match budget {
            Some(budget) => TcpStream::connect_timeout(
                addr,
                budget.mul_f64(weight / total_weight).max(MIN_PER_ADDRESS),
            ),
            None => TcpStream::connect(addr),
        };
        match attempt {
            Ok(stream) => return Ok(stream),
            Err(error) if address_specific(&error) => last = Some(ureq::Error::Io(error)),
            Err(error)
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
            Err(error) => return Err(error.into()),
        }
        weight /= 2.0;
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::ConnectionRefused, "Connection refused").into()
    }))
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
    interrupt: Arc<AtomicBool>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl std::fmt::Debug for KeepaliveTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeepaliveTransport")
            .field("addr", &self.stream.peer_addr().ok())
            .finish()
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
    match error.kind() {
        io::ErrorKind::WouldBlock => true,
        io::ErrorKind::TimedOut => cfg!(windows),
        _ => false,
    }
}

impl Transport for KeepaliveTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    /// Writes in slices of at most [`CANCEL_POLL`] and chunks of at most [`MAX_WRITE`] bytes,
    /// resuming at the offset already sent, so an interrupt is noticed while a slow peer holds
    /// the write and no byte is ever sent twice.
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let budget = timeout.not_zero().map(|after| *after);
        let started = Instant::now();
        let mut sent = 0;
        while sent < amount {
            super::check(&self.interrupt)?;
            let mut slice = CANCEL_POLL;
            if let Some(budget) = budget {
                let remaining = budget.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(ureq::Error::Timeout(timeout.reason));
                }
                slice = slice.min(remaining);
            }
            if self.write_timeout != Some(slice) {
                self.stream.set_write_timeout(Some(slice))?;
                self.write_timeout = Some(slice);
            }
            let output = &self.buffers.output()[sent..amount.min(sent + MAX_WRITE)];
            match self.stream.write(output) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(count) => sent += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // An expired slice: look at the interrupt and the phase budget, then resume.
                Err(error) if expired(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        arm(
            &self.stream,
            &mut self.read_timeout,
            timeout,
            TcpStream::set_read_timeout,
        )?;
        let amount = self
            .stream
            .read(self.buffers.input_append_buf())
            .map_err(|error| io_failure(error, timeout))?;
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

//! Request-scoped cancellation and deadlines below TLS, including partial WebSocket frames.
use super::{Interrupted, Patience, stream::StalledStream};
use anyhow::{Context, Result, bail, ensure};
use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tungstenite::{WebSocket, handshake::client::Request, stream::MaybeTlsStream};

pub(super) type Socket = WebSocket<MaybeTlsStream<Transport>>;

/// One setup budget spans TCP, the proxy tunnel, TLS and the HTTP upgrade. System DNS
/// and a blocking TCP connect cannot be interrupted in flight.
pub(super) fn upgrade(
    url: &url::Url,
    request: Request,
    timeout: u64,
    interrupt: &Arc<AtomicBool>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> Result<Socket> {
    let budget = Budget::new(timeout, Arc::clone(interrupt));
    budget.remaining()?;
    let host = url.host_str().context("WebSocket URL has no host")?;
    let port = url
        .port_or_known_default()
        .context("WebSocket URL has no port")?;
    let tcp = super::ws_proxy::open(host, port, &|| budget.remaining())?;
    tcp.set_nodelay(true)?;
    let result =
        tungstenite::client_tls_with_config(request, Transport::new(tcp, Some(budget)), None, None);
    if interrupt.load(Ordering::Relaxed) {
        return Err(Interrupted.into());
    }
    let (mut socket, _) = result.map_err(|error| match error {
        tungstenite::handshake::HandshakeError::Failure(error) => anyhow::Error::from(error),
        tungstenite::handshake::HandshakeError::Interrupted(_) => {
            anyhow::anyhow!("WebSocket handshake interrupted")
        }
    })?;
    let tcp = match socket.get_mut() {
        MaybeTlsStream::Plain(tcp) => tcp,
        MaybeTlsStream::Rustls(tls) => &mut tls.sock,
        _ => bail!("unsupported WebSocket TLS transport"),
    };
    tcp.finish()?;
    tcp.set_read_timeout(read_timeout)?;
    tcp.set_write_timeout(write_timeout)?;
    Ok(socket)
}

const POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(super) struct Budget {
    started: Instant,
    limit: Option<Duration>,
    interrupt: Arc<AtomicBool>,
}
impl Budget {
    pub(super) fn new(timeout: u64, interrupt: Arc<AtomicBool>) -> Self {
        Self {
            started: Instant::now(),
            limit: (timeout > 0).then(|| Duration::from_secs(timeout)),
            interrupt,
        }
    }
    pub(super) fn remaining(&self) -> Result<Option<Duration>> {
        if self.interrupt.load(Ordering::Relaxed) {
            return Err(Interrupted.into());
        }
        let remaining = self
            .limit
            .map(|limit| limit.saturating_sub(self.started.elapsed()));
        ensure!(
            !remaining.is_some_and(|left| left.is_zero()),
            "WebSocket setup timed out"
        );
        Ok(remaining)
    }
}

#[derive(Debug)]
pub(super) struct Transport {
    tcp: TcpStream,
    // Detached between requests so cached sockets never retain an old cancellation flag.
    budget: Option<Budget>,
    streaming: Option<(Arc<AtomicBool>, Patience)>,
}
impl std::ops::Deref for Transport {
    type Target = TcpStream;
    fn deref(&self) -> &TcpStream {
        &self.tcp
    }
}
impl Transport {
    pub(super) fn new(tcp: TcpStream, budget: Option<Budget>) -> Self {
        Self {
            tcp,
            budget,
            streaming: None,
        }
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        if let Some(budget) = &self.budget {
            budget.remaining()?;
        }
        self.budget = None;
        Ok(())
    }
    pub(super) fn bind(&mut self, interrupt: Arc<AtomicBool>, patience: Patience) {
        self.streaming = Some((interrupt, patience));
    }
    pub(super) fn detach(&mut self) {
        self.streaming = None;
    }
    fn poll<T>(
        &mut self,
        write: bool,
        mut operation: impl FnMut(&mut TcpStream) -> io::Result<T>,
    ) -> io::Result<T> {
        loop {
            let left = if let Some((interrupt, patience)) = &self.streaming {
                if interrupt.load(Ordering::Relaxed) {
                    return Err(io::Error::other(Interrupted));
                }
                let left = patience.remaining();
                if left.is_some_and(|left| left.is_zero()) {
                    return Err(io::Error::other(StalledStream));
                }
                left
            } else if let Some(budget) = &self.budget {
                budget.remaining().map_err(io::Error::other)?
            } else {
                return operation(&mut self.tcp);
            };
            let slice = Some(left.unwrap_or(POLL).min(POLL));
            if write {
                self.tcp.set_write_timeout(slice)?;
            } else {
                self.tcp.set_read_timeout(slice)?;
            }
            match operation(&mut self.tcp) {
                Err(error)
                    if (write || self.streaming.is_none())
                        && matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}
impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.poll(false, |tcp| tcp.read(buf))
    }
}
impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.poll(true, |tcp| tcp.write(buf))
    }
    fn flush(&mut self) -> io::Result<()> {
        self.poll(true, Write::flush)
    }
}

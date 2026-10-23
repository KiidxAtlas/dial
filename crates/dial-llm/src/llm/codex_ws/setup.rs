//! Request-scoped cancellation and deadlines below TLS, including partial WebSocket frames.
use super::*;
use std::io::{self, Read, Write};

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
            "Codex WebSocket setup timed out"
        );
        Ok(remaining)
    }
}

#[derive(Debug)]
pub(super) struct Transport {
    tcp: TcpStream,
    // Detached between requests so cached sockets never retain an old cancellation flag.
    budget: Option<Budget>,
    streaming: Option<(Arc<AtomicBool>, super::super::Patience)>,
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
    pub(super) fn bind(&mut self, interrupt: Arc<AtomicBool>, patience: super::super::Patience) {
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
                    return Err(io::Error::other(super::super::stream::StalledStream));
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

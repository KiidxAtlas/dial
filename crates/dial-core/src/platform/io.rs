use std::io::{self, Read};
use std::time::Duration;

/// A pipe or terminal handle: `AsFd` on Unix, `AsHandle` on Windows.
#[cfg(unix)]
pub trait Pipe: std::os::fd::AsFd {}
#[cfg(unix)]
impl<T: std::os::fd::AsFd + ?Sized> Pipe for T {}
/// A pipe or terminal handle: `AsFd` on Unix, `AsHandle` on Windows.
#[cfg(windows)]
pub trait Pipe: std::os::windows::io::AsHandle {}
#[cfg(windows)]
impl<T: std::os::windows::io::AsHandle + ?Sized> Pipe for T {}

/// Restore blocking mode on an adopted output descriptor (Unix: clear `O_NONBLOCK`; Windows:
/// the handle mode is not changed).
pub fn set_blocking(pipe: &impl Pipe) -> io::Result<()> {
    super::imp::io::set_blocking(pipe)
}

/// Prepare `pipe` for [`read_nonblocking`] (Unix: `O_NONBLOCK`; Windows: verifies the handle is
/// a pipe, whose reads are then bounded by `PeekNamedPipe`).
pub fn set_nonblocking(pipe: &impl Pipe) -> io::Result<()> {
    super::imp::io::set_nonblocking(pipe)
}

/// Read what is available without blocking: `Ok(0)` is EOF and `WouldBlock` means no data yet.
/// `reader` must have been passed to [`set_nonblocking`].
pub fn read_nonblocking<R: Read + Pipe>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    super::imp::io::read_nonblocking(reader, buffer)
}

/// Wait up to `timeout` until `pipe` has data or reached EOF (Unix: `poll(POLLIN)`). Returns
/// whether it became readable; an interrupted wait returns `false`.
pub fn wait_readable(pipe: &impl Pipe, timeout: Duration) -> io::Result<bool> {
    super::imp::io::wait_readable(pipe, timeout)
}

/// Wait up to `timeout` until a pipe can accept a write. Windows anonymous pipes expose no
/// writable readiness signal; this returns `Unsupported` rather than reporting false readiness.
pub fn wait_writable(pipe: &impl Pipe, timeout: Duration) -> io::Result<bool> {
    super::imp::io::wait_writable(pipe, timeout)
}

/// A bounded asynchronous writer for synchronous Windows pipe handles. The worker owns the
/// writer and performs blocking writes; callers only enqueue owned bytes.
#[cfg(windows)]
pub fn duplicate_stdout() -> io::Result<std::fs::File> {
    super::imp::io::duplicate_stdout()
}
/// A duplicated stdin handle with console-aware nonblocking reads.
#[cfg(windows)]
pub use super::imp::console::duplicate_stdin_reader;
#[cfg(windows)]
pub struct QueuedWriter<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> {
    state: std::sync::Arc<(
        std::sync::Mutex<WriterState>,
        std::sync::Condvar,
        std::sync::atomic::AtomicBool,
    )>,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    capacity: usize,
    _writer: std::marker::PhantomData<W>,
}
#[cfg(windows)]
struct WriterState {
    queue: std::collections::VecDeque<Vec<u8>>,
    bytes: usize,
    delivered: usize,
    closed: bool,
    failure: Option<(io::ErrorKind, String)>,
}
#[cfg(windows)]
impl<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> QueuedWriter<W> {
    pub fn new(mut writer: W, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let state = std::sync::Arc::new((
            std::sync::Mutex::new(WriterState {
                queue: std::collections::VecDeque::new(),
                bytes: 0,
                delivered: 0,
                closed: false,
                failure: None,
            }),
            std::sync::Condvar::new(),
            std::sync::atomic::AtomicBool::new(false),
        ));
        let worker_state = state.clone();
        let worker = std::thread::spawn(move || {
            loop {
                let (lock, ready, abort) = &*worker_state;
                let mut current = lock.lock().unwrap_or_else(|p| p.into_inner());
                while current.queue.is_empty() && !current.closed {
                    current = ready.wait(current).unwrap_or_else(|p| p.into_inner());
                }
                if current.queue.is_empty() && current.closed {
                    return;
                }
                let bytes = current.queue.pop_front().unwrap();
                drop(current);
                let mut delivered = 0;
                let result = loop {
                    if delivered == bytes.len() {
                        break Ok(());
                    }
                    if abort.load(std::sync::atomic::Ordering::Acquire) {
                        break Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "queued pipe writer cancelled",
                        ));
                    }
                    match writer.write(&bytes[delivered..]) {
                        Ok(0) => {
                            break Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "queued pipe writer wrote zero bytes",
                            ));
                        }
                        Ok(n) => delivered += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => break Err(e),
                    }
                };
                let mut current = lock.lock().unwrap_or_else(|p| p.into_inner());
                current.delivered += delivered;
                current.bytes -= bytes.len();
                if let Err(error) = result {
                    current.failure = Some((error.kind(), error.to_string()));
                    current.bytes -= current.queue.iter().map(Vec::len).sum::<usize>();
                    current.queue.clear();
                    current.closed = true;
                }
                ready.notify_all();
            }
        });
        Self {
            state,
            worker: std::sync::Mutex::new(Some(worker)),
            capacity,
            _writer: std::marker::PhantomData,
        }
    }
    /// Enqueues up to the available bounded capacity. Returned bytes are accepted, not yet delivered.
    pub fn try_write(&self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let (lock, ready, _) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((kind, message)) = &state.failure {
            return Err(io::Error::new(*kind, message.clone()));
        }
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "queued pipe writer is closed",
            ));
        }
        let count = bytes.len().min(self.capacity.saturating_sub(state.bytes));
        if count == 0 || state.queue.len() >= 128 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        state.queue.push_back(bytes[..count].to_vec());
        state.bytes += count;
        ready.notify_one();
        Ok(count)
    }
    /// Number of bytes successfully written to the underlying writer.
    pub fn delivered_bytes(&self) -> usize {
        self.state
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .delivered
    }
    /// Waits for accepted bytes to be delivered; returns false on timeout.
    pub fn wait_until_drained(&self, timeout: Duration) -> io::Result<bool> {
        let deadline = std::time::Instant::now() + timeout;
        let (lock, ready, _) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some((kind, message)) = &state.failure {
                return Err(io::Error::new(*kind, message.clone()));
            }
            if state.bytes == 0 {
                return Ok(true);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            let (next, result) = ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            state = next;
            if result.timed_out() && state.bytes != 0 {
                return Ok(false);
            }
        }
    }
    /// Stops accepting input without waiting. The worker drains all accepted bytes, then
    /// drops the owned writer to signal EOF. Callers can continue servicing output and
    /// cancellation while delivery completes.
    pub fn close(&self) {
        let (lock, ready, _) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        ready.notify_all();
    }
    /// Closes the queue, drains accepted bytes, then drops the owned writer to signal EOF.
    pub fn finish(&self, timeout: Duration) -> io::Result<bool> {
        self.close();
        if !self.wait_until_drained(timeout)? {
            self.cancel_worker();
            return Ok(false);
        }
        self.join_worker();
        Ok(true)
    }
    fn cancel_worker(&self) {
        use std::{os::windows::io::AsRawHandle, time::Instant};
        self.state
            .2
            .store(true, std::sync::atomic::Ordering::Release);
        {
            let mut state = self.state.0.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
            state.bytes -= state.queue.iter().map(Vec::len).sum::<usize>();
            state.queue.clear();
        }
        self.state.1.notify_all();
        let mut worker = self.worker.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(handle) = worker.as_ref() {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !handle.is_finished() && Instant::now() < deadline {
                unsafe {
                    windows_sys::Win32::System::IO::CancelSynchronousIo(handle.as_raw_handle() as _);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                if let Some(handle) = worker.take() {
                    let _ = handle.join();
                }
            }
        }
    }
    fn join_worker(&self) {
        if let Some(worker) = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = worker.join();
        }
    }
}
/// Nonblocking `Write` integration: accepted bytes are queued; `flush` returns `WouldBlock`
/// until the worker has delivered them, allowing callers to enforce their own deadline.
#[cfg(windows)]
impl<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> std::io::Write
    for QueuedWriter<W>
{
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.try_write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.wait_until_drained(Duration::ZERO)? {
            Ok(())
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
}
#[cfg(windows)]
impl<W: std::io::Write + Send + 'static + std::os::windows::io::AsRawHandle> Drop
    for QueuedWriter<W>
{
    fn drop(&mut self) {
        let (lock, ready, _) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.bytes -= state.queue.iter().map(Vec::len).sum::<usize>();
        state.queue.clear();
        drop(state);
        ready.notify_all();
        self.cancel_worker();
    }
}

#[cfg(all(test, windows))]
mod tests;

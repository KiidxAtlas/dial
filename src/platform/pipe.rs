//! Bounded, cancellable polling of subprocess pipes on Unix and Windows.
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, RawHandle};
#[cfg(unix)]
pub type Handle = RawFd;
#[cfg(windows)]
pub type Handle = RawHandle;

pub struct Reader<T> {
    inner: T,
}
#[cfg(unix)]
impl<T: Read + AsRawFd> Reader<T> {
    pub fn new(inner: T) -> io::Result<Self> {
        nonblocking(inner.as_raw_fd())?;
        Ok(Self { inner })
    }
    pub fn handle(&self) -> Handle {
        self.inner.as_raw_fd()
    }
}
#[cfg(unix)]
impl<T: Read> Read for Reader<T> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.inner.read(b)
    }
}
#[cfg(windows)]
impl<T: Read + AsRawHandle> Reader<T> {
    pub fn new(inner: T) -> io::Result<Self> {
        Ok(Self { inner })
    }
    pub fn handle(&self) -> Handle {
        self.inner.as_raw_handle()
    }
}
#[cfg(windows)]
impl<T: Read + AsRawHandle> Read for Reader<T> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        use windows_sys::Win32::{
            Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED},
            System::Pipes::PeekNamedPipe,
        };
        if b.is_empty() {
            return Ok(0);
        }
        let mut available = 0;
        let ok = unsafe {
            PeekNamedPipe(
                self.inner.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.raw_os_error().map(|x| x as u32),
                Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
            ) {
                return Ok(0);
            }
            return Err(error);
        }
        if available == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let count = b.len().min(available as usize);
        self.inner.read(&mut b[..count])
    }
}
#[cfg(unix)]
pub struct Writer<T> {
    inner: T,
}
#[cfg(unix)]
impl<T: Write + AsRawFd> Writer<T> {
    pub fn new(inner: T) -> io::Result<Self> {
        nonblocking(inner.as_raw_fd())?;
        Ok(Self { inner })
    }
    pub fn handle(&self) -> Handle {
        self.inner.as_raw_fd()
    }
}
#[cfg(unix)]
impl<T: Write> Write for Writer<T> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.inner.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
#[cfg(windows)]
pub struct Writer<T> {
    outgoing: std::sync::mpsc::SyncSender<Vec<u8>>,
    replies: std::sync::mpsc::Receiver<io::Result<usize>>,
    pending: bool,
    _type: std::marker::PhantomData<T>,
}
#[cfg(windows)]
impl<T: Write + Send + 'static> Writer<T> {
    pub fn new(mut inner: T) -> io::Result<Self> {
        let (outgoing, incoming) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let (reply, replies) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("dial-pipe-writer".into())
            .spawn(move || {
                while let Ok(bytes) = incoming.recv() {
                    let result = inner.write_all(&bytes).map(|()| bytes.len());
                    let failed = result.is_err();
                    if reply.send(result).is_err() || failed {
                        break;
                    }
                }
            })?;
        Ok(Self {
            outgoing,
            replies,
            pending: false,
            _type: std::marker::PhantomData,
        })
    }
    pub fn handle(&self) -> Handle {
        std::ptr::null_mut()
    }
}
#[cfg(windows)]
impl<T: Write + Send + 'static> Write for Writer<T> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        use std::sync::mpsc::TryRecvError;
        if b.is_empty() {
            return Ok(0);
        }
        if self.pending {
            match self.replies.try_recv() {
                Ok(result) => {
                    self.pending = false;
                    return result;
                }
                Err(TryRecvError::Empty) => return Err(io::ErrorKind::WouldBlock.into()),
                Err(TryRecvError::Disconnected) => return Err(io::ErrorKind::BrokenPipe.into()),
            }
        }
        // At most one 8 KiB write is outstanding. The caller keeps checking its
        // deadline; terminating the Job Object unblocks the worker's OS write.
        self.outgoing
            .send(b[..b.len().min(8192)].to_vec())
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        self.pending = true;
        Err(io::ErrorKind::WouldBlock.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[cfg(unix)]
fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn wait(handle: Handle, writable: bool, millis: i32) {
    #[cfg(unix)]
    {
        let mut poll = libc::pollfd {
            fd: handle,
            events: if writable {
                libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        };
        unsafe {
            libc::poll(&mut poll, 1, millis);
        }
    }
    #[cfg(windows)]
    {
        let _ = (handle, writable);
        std::thread::sleep(std::time::Duration::from_millis(millis.max(0) as u64));
    }
}

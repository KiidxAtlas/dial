use crate::platform::io::Pipe;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::time::Duration;

pub(in crate::platform) fn set_nonblocking(pipe: &impl Pipe) -> io::Result<()> {
    let fd = pipe.as_fd().as_raw_fd();
    // SAFETY: fcntl only reads/updates status flags of a live descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(in crate::platform) fn set_blocking(pipe: &impl Pipe) -> io::Result<()> {
    let fd = pipe.as_fd().as_raw_fd();
    // SAFETY: fcntl only reads/updates status flags of a live descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(in crate::platform) fn read_nonblocking<R: Read + Pipe>(
    reader: &mut R,
    buffer: &mut [u8],
) -> io::Result<usize> {
    reader.read(buffer)
}

fn wait(pipe: &impl Pipe, events: libc::c_short, timeout: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd: pipe.as_fd().as_raw_fd(),
        events,
        revents: 0,
    };
    let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one initialized pollfd and a finite timeout.
    let ready = unsafe { libc::poll(&mut poll, 1, millis) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        };
    }
    Ok(ready > 0)
}

pub(in crate::platform) fn wait_readable(pipe: &impl Pipe, timeout: Duration) -> io::Result<bool> {
    wait(pipe, libc::POLLIN, timeout)
}

pub(in crate::platform) fn wait_writable(pipe: &impl Pipe, timeout: Duration) -> io::Result<bool> {
    wait(pipe, libc::POLLOUT, timeout)
}

pub(in crate::platform) fn wait_socket_writable(
    socket: &std::net::TcpStream,
    timeout: Duration,
) -> io::Result<bool> {
    wait(socket, libc::POLLOUT, timeout)
}

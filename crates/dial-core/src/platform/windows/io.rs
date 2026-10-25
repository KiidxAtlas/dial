//! Anonymous pipe reads are bounded by a peek; an empty live pipe is not EOF.
use std::{
    fs::File,
    io::{self, Read},
    os::windows::io::{AsRawHandle, FromRawHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, ERROR_NO_DATA,
        ERROR_PIPE_NOT_CONNECTED, HANDLE,
    },
    System::{
        Console::{GetStdHandle, STD_OUTPUT_HANDLE},
        Pipes::{GetNamedPipeInfo, PeekNamedPipe},
        Threading::GetCurrentProcess,
    },
};

pub(in crate::platform) fn duplicate_stdout() -> io::Result<File> {
    let source = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if source.is_null() || source == -1isize as HANDLE {
        return Err(io::Error::last_os_error());
    }
    let mut duplicate: HANDLE = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(duplicate.cast()) })
}

fn available(pipe: &impl crate::platform::io::Pipe) -> io::Result<Option<usize>> {
    let mut bytes = 0;
    if unsafe {
        PeekNamedPipe(
            pipe.as_handle().as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut bytes,
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Ok(Some(bytes as usize));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|v| v as u32) {
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA) => Ok(None),
        _ => Err(error),
    }
}
pub(in crate::platform) fn set_nonblocking(
    pipe: &impl crate::platform::io::Pipe,
) -> io::Result<()> {
    // We do not change a shared handle's mode; read_nonblocking performs the bounded peek.
    if unsafe {
        GetNamedPipeInfo(
            pipe.as_handle().as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(in crate::platform) fn read_nonblocking<R: Read + crate::platform::io::Pipe>(
    reader: &mut R,
    buffer: &mut [u8],
) -> io::Result<usize> {
    if buffer.is_empty() {
        return Ok(0);
    }
    match available(reader)? {
        None => Ok(0),
        Some(0) => Err(io::ErrorKind::WouldBlock.into()),
        Some(count) => {
            let length = count.min(buffer.len());
            reader.read(&mut buffer[..length])
        }
    }
}
pub(in crate::platform) fn wait_readable(
    pipe: &impl crate::platform::io::Pipe,
    timeout: Duration,
) -> io::Result<bool> {
    let start = Instant::now();
    loop {
        if available(pipe)?.is_none_or(|count| count > 0) {
            return Ok(true);
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(false);
        }
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}
pub(in crate::platform) fn set_blocking(pipe: &impl crate::platform::io::Pipe) -> io::Result<()> {
    // Windows pipe handles have no shared per-handle blocking flag to toggle.
    if unsafe {
        GetNamedPipeInfo(
            pipe.as_handle().as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub(in crate::platform) fn wait_writable(
    _pipe: &impl crate::platform::io::Pipe,
    _timeout: Duration,
) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows anonymous pipes do not expose writable readiness; use QueuedWriter",
    ))
}

pub(in crate::platform) fn wait_socket_writable(
    socket: &std::net::TcpStream,
    timeout: Duration,
) -> io::Result<bool> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        POLLWRNORM, SOCKET_ERROR, WSAGetLastError, WSAPOLLFD, WSAPoll,
    };
    let mut poll = WSAPOLLFD {
        fd: socket.as_raw_socket() as _,
        events: POLLWRNORM,
        revents: 0,
    };
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one initialized WSAPOLLFD naming a socket borrowed for the call; finite timeout.
    let ready = unsafe { WSAPoll(&mut poll, 1, millis) };
    if ready == SOCKET_ERROR {
        // SAFETY: reads this thread's last Winsock error; no arguments.
        let error = io::Error::from_raw_os_error(unsafe { WSAGetLastError() });
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(error)
        };
    }
    Ok(ready > 0)
}

//! Nonblocking input for duplicated Windows standard input.
use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read},
    os::windows::io::{AsRawHandle, FromRawHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{FILE_TYPE_DISK, FILE_TYPE_PIPE, GetFileType},
    System::{
        Console::{
            ENABLE_LINE_INPUT, GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle,
            INPUT_RECORD, KEY_EVENT, PeekConsoleInputW, ReadConsoleInputW, ReadConsoleW,
            STD_INPUT_HANDLE,
        },
        Threading::{GetCurrentProcess, WaitForSingleObject},
    },
};

/// An owned stdin handle classified as a console or a pipe. Console readiness is tracked
/// separately from byte readiness because cooked ReadConsoleW only completes at a line boundary.
pub struct StdInput {
    file: File,
    console: bool,
    pipe: bool,
    mode: u32,
    pending: VecDeque<u8>,
    eof: bool,
    high_surrogate: Option<u16>,
}
impl StdInput {
    fn from_file(file: File, require_console: bool) -> io::Result<Self> {
        let mut mode = 0;
        let console = unsafe { GetConsoleMode(file.as_raw_handle() as HANDLE, &mut mode) } != 0;
        if require_console && !console {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "CONIN$ is not a console input handle",
            ));
        }
        let file_type = unsafe { GetFileType(file.as_raw_handle() as HANDLE) };
        let pipe = !console && file_type == FILE_TYPE_PIPE;
        if !console && !pipe && file_type != FILE_TYPE_DISK {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdin handle is neither a console, pipe, nor disk file",
            ));
        }
        Ok(Self {
            file,
            console,
            pipe,
            mode,
            pending: VecDeque::new(),
            eof: false,
            high_surrogate: None,
        })
    }
    fn fill_console(&mut self) -> io::Result<bool> {
        if self.eof {
            return Ok(true);
        }
        let handle = self.file.as_raw_handle() as HANDLE;
        // The console mode can change after open (e.g. a TUI entering raw input), so read it per fill.
        if unsafe { GetConsoleMode(handle, &mut self.mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut count = 0;
        if unsafe { GetNumberOfConsoleInputEvents(handle, &mut count) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if count == 0 {
            return Ok(false);
        }
        let mut events = vec![INPUT_RECORD::default(); count as usize];
        let mut peeked = 0;
        if unsafe { PeekConsoleInputW(handle, events.as_mut_ptr(), count, &mut peeked) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let line_mode = self.mode & ENABLE_LINE_INPUT != 0;
        // Cooked ReadConsoleW blocks until Enter and raw ReadConsoleW blocks while only
        // non-character records are queued, so only read once a usable record is present.
        let ready = events[..peeked as usize].iter().any(|event| {
            if event.EventType != KEY_EVENT as u16 {
                return false;
            }
            let key = unsafe { event.Event.KeyEvent };
            key.bKeyDown != 0
                && if line_mode {
                    key.wVirtualKeyCode == 0x0d
                } else {
                    (unsafe { key.uChar.UnicodeChar }) != 0
                }
        });
        if !ready {
            if !line_mode {
                let mut discard = vec![INPUT_RECORD::default(); count as usize];
                let mut consumed = 0;
                if unsafe { ReadConsoleInputW(handle, discard.as_mut_ptr(), count, &mut consumed) }
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            return Ok(false);
        }
        let mut units = [0u16; 512];
        let mut got = 0;
        if unsafe {
            ReadConsoleW(
                handle,
                units.as_mut_ptr().cast(),
                units.len() as u32,
                &mut got,
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if got == 0 {
            self.eof = true;
            return Ok(true);
        }
        for unit in units[..got as usize].iter().copied() {
            if unit == 0x1a {
                self.eof = true;
                break;
            }
            if (0xd800..=0xdbff).contains(&unit) {
                // An unpaired high surrogate that is followed by another high surrogate becomes U+FFFD.
                if self.high_surrogate.replace(unit).is_some() {
                    self.pending.extend("\u{fffd}".as_bytes());
                }
                continue;
            }
            let character = if (0xdc00..=0xdfff).contains(&unit) {
                match self.high_surrogate.take() {
                    Some(high) => char::from_u32(
                        0x10000 + (((high as u32 - 0xd800) << 10) | (unit as u32 - 0xdc00)),
                    )
                    .unwrap_or('\u{fffd}'),
                    None => '\u{fffd}',
                }
            } else {
                if self.high_surrogate.take().is_some() {
                    self.pending.extend("\u{fffd}".as_bytes());
                }
                char::from_u32(unit as u32).unwrap_or('\u{fffd}')
            };
            let mut buf = [0; 4];
            self.pending
                .extend(character.encode_utf8(&mut buf).as_bytes());
        }
        Ok(true)
    }
    /// Read available bytes. Empty live input returns WouldBlock; EOF returns zero.
    pub fn read_nonblocking(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.console {
            if self.pending.is_empty() {
                self.fill_console()?;
            }
            if !self.pending.is_empty() {
                let n = buffer.len().min(self.pending.len());
                for out in &mut buffer[..n] {
                    *out = self.pending.pop_front().unwrap();
                }
                return Ok(n);
            }
            // A console handle must never reach the blocking read below.
            return if self.eof {
                Ok(0)
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            };
        }
        if self.pipe {
            super::io::read_nonblocking(&mut self.file, buffer)
        } else {
            self.file.read(buffer)
        }
    }
    /// Wait for bytes or EOF, respecting cooked console line readiness.
    pub fn wait_readable(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.pipe {
            return super::io::wait_readable(&self.file, timeout);
        }
        if !self.console {
            return Ok(true);
        }
        if self.eof || !self.pending.is_empty() {
            return Ok(true);
        }
        let start = Instant::now();
        loop {
            if self.fill_console()? {
                return Ok(true);
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(false);
            }
            let handle = self.file.as_raw_handle() as HANDLE;
            let mut queued = 0;
            if unsafe { GetNumberOfConsoleInputEvents(handle, &mut queued) } == 0 {
                return Err(io::Error::last_os_error());
            }
            if queued > 0 {
                // The handle stays signaled while any record is queued (typed keys before Enter,
                // resize or focus records), so waiting on it would spin. Poll at a short interval.
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
                continue;
            }
            let result = unsafe {
                WaitForSingleObject(handle, remaining.as_millis().min(u32::MAX as u128) as u32)
            };
            if result == WAIT_OBJECT_0 {
                continue;
            }
            if result == WAIT_TIMEOUT {
                return Ok(false);
            }
            return Err(io::Error::last_os_error());
        }
    }
}
/// Duplicate the current process stdin and classify its handle for safe nonblocking reads.
pub fn duplicate_stdin_reader() -> io::Result<StdInput> {
    let source = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
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
    StdInput::from_file(unsafe { File::from_raw_handle(duplicate.cast()) }, false)
}
impl Read for StdInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_nonblocking(buffer)
    }
}

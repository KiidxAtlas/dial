//! Windows implementations of platform primitives.

#[path = "windows_console.rs"]
pub(super) mod console;
#[path = "windows_fs.rs"]
pub(super) mod fs;
#[path = "windows_io.rs"]
pub(super) mod io;
#[path = "windows_process.rs"]
pub(super) mod process;
#[path = "windows_pty.rs"]
pub(super) mod pty;

pub(super) mod time {
    pub(in crate::platform) fn local_strftime(fmt: &str) -> String {
        use windows_sys::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};
        let mut t = SYSTEMTIME::default();
        unsafe { GetLocalTime(&mut t) }
        let mut out = String::new();
        let mut i = fmt.chars();
        while let Some(c) = i.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match i.next() {
                Some('Y') => out.push_str(&format!("{:04}", t.wYear)),
                Some('m') => out.push_str(&format!("{:02}", t.wMonth)),
                Some('d') => out.push_str(&format!("{:02}", t.wDay)),
                Some('H') => out.push_str(&format!("{:02}", t.wHour)),
                Some('M') => out.push_str(&format!("{:02}", t.wMinute)),
                Some('S') => out.push_str(&format!("{:02}", t.wSecond)),
                Some('%') => out.push('%'),
                _ => return String::new(),
            }
        }
        out
    }
}

pub(super) mod sys {
    use std::{io, sync::OnceLock};
    use windows_sys::Win32::System::{
        Console::SetConsoleCtrlHandler,
        SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW},
        Threading::ExitProcess,
    };
    static HANDLER: OnceLock<fn()> = OnceLock::new();
    unsafe extern "system" fn callback(_: u32) -> i32 {
        match HANDLER.get() {
            Some(handler) => {
                handler();
                1
            }
            None => 0,
        }
    }
    pub(in crate::platform) fn hostname() -> io::Result<String> {
        // The physical DNS host name, not the NetBIOS COMPUTERNAME (which may be unset under services).
        let mut len = 0u32;
        unsafe { GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut len) };
        let mut buffer = vec![0u16; len as usize];
        if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut len) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(String::from_utf16_lossy(&buffer[..len as usize]))
    }
    pub(in crate::platform) fn on_interrupt(handler: fn()) {
        // First registration wins, matching the Unix handler; repeat calls must not add callbacks.
        if HANDLER.set(handler).is_err() {
            return;
        }
        unsafe {
            SetConsoleCtrlHandler(Some(callback), 1);
        }
    }
    pub(in crate::platform) fn exit_now(code: i32) -> ! {
        unsafe { ExitProcess(code as u32) }
    }
}

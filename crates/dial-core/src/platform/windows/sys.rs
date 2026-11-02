use std::{io, sync::OnceLock};
use windows_sys::Win32::System::{
    Console::SetConsoleCtrlHandler,
    SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW},
    Threading::ExitProcess,
};
static HANDLER: OnceLock<fn()> = OnceLock::new();
/// Dispatches a control notification using the statically registered handler.
///
/// # Safety
/// Windows must invoke this with the system callback ABI. The event code has no
/// validity restrictions, and no caller-owned memory is accessed.
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
    // SAFETY: a null buffer with zero capacity requests the required UTF-16 length;
    // len is writable and no buffer bytes are accessed in this sizing call.
    unsafe { GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut len) };
    let mut buffer = vec![0u16; len as usize];
    // SAFETY: buffer has len writable UTF-16 elements as requested by the sizing call;
    // Windows reports a larger required size as failure rather than overrunning it.
    if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(String::from_utf16_lossy(&buffer[..len as usize]))
}
pub(in crate::platform) fn on_interrupt(handler: fn()) {
    // First registration wins, matching the Unix handler; repeat calls must not add callbacks.
    if HANDLER.set(handler).is_err() {
        return;
    }
    // SAFETY: callback has the required system ABI and static lifetime; HANDLER is
    // initialized before registration and remains live for all later callbacks.
    unsafe {
        SetConsoleCtrlHandler(Some(callback), 1);
    }
}
pub(in crate::platform) fn exit_now(code: i32) -> ! {
    // SAFETY: ExitProcess takes only an exit code and never returns.
    unsafe { ExitProcess(code as u32) }
}

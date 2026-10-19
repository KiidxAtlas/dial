use std::io;
use std::sync::OnceLock;

pub(in crate::platform) fn hostname() -> io::Result<String> {
    let mut name = [0u8; 256];
    // SAFETY: the buffer is writable for its full length.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf8_lossy(&name[..end]).into_owned())
}

static HANDLER: OnceLock<fn()> = OnceLock::new();

extern "C" fn dispatch(_: libc::c_int) {
    if let Some(handler) = HANDLER.get() {
        handler();
    }
}

pub(in crate::platform) fn on_interrupt(handler: fn()) {
    if HANDLER.set(handler).is_err() {
        return;
    }
    // SAFETY: a zeroed sigaction with an empty mask and a plain handler is valid.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = dispatch as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

pub(in crate::platform) fn exit_now(code: i32) -> ! {
    // SAFETY: _exit never returns and has no preconditions.
    unsafe { libc::_exit(code) }
}

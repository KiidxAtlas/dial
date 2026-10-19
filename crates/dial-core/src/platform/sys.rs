use std::io;

/// The host name (Unix: `gethostname`; Windows: the physical DNS host name).
pub fn hostname() -> io::Result<String> {
    super::imp::sys::hostname()
}

/// Run `handler` on every interactive interrupt (Unix: `SIGINT` via `sigaction`; Windows:
/// Ctrl+C / Ctrl+Break console events). `handler` runs in signal context on Unix and on a
/// console-control thread on Windows: it must only touch atomics or call [`exit_now`].
pub fn on_interrupt(handler: fn()) {
    super::imp::sys::on_interrupt(handler)
}

/// Terminate immediately without running destructors or exit handlers (`_exit`/`ExitProcess`).
pub fn exit_now(code: i32) -> ! {
    super::imp::sys::exit_now(code)
}

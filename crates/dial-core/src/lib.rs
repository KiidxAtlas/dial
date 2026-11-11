pub mod build_identity;
pub mod credentials;
pub mod output;
pub mod platform;
pub mod prose;
pub mod runtime;
pub mod workspace_copy;

/// Waits (bounded) until buffered stdout has been written. Binaries call this on every exit path.
pub fn flush_stdout_on_exit() -> std::io::Result<()> {
    output::drain_stdout(Some(std::time::Duration::from_secs(5)))
}

/// Exit status for output cut short by a closed reader, as a shell reports a SIGPIPE death.
pub const CLOSED_STDOUT_STATUS: i32 = 141;

/// `dial config | head` closes stdout early; Rust ignores SIGPIPE, so `println!` then panics with
/// "failed printing to stdout: Broken pipe". Turn that panic into a quiet exit, as other command
/// line tools behave. SIGPIPE itself stays ignored: Dial writes to child pipes that may close.
pub fn exit_quietly_on_closed_stdout() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or_default();
        if output::is_closed_stdout_panic(message) {
            std::process::exit(CLOSED_STDOUT_STATUS);
        }
        previous(info);
    }));
}

/// Selects rustls's process-wide crypto provider. If a dependency enables a second provider
/// (`aws-lc-rs`), rustls refuses to choose and panics on the first TLS handshake. The AWS SDK's
/// HTTP client is built on `ring` as well, so the default graph has one provider. Idempotent;
/// binaries call it first, and every HTTP client builder calls it too so library and test entry
/// points are covered.
pub fn install_tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err means another provider was installed first, which is equally usable.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Lowercase hexadecimal encoding of `bytes`, two digits per byte.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

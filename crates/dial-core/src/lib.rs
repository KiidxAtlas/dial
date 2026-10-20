pub mod output;
pub mod platform;
pub mod prose;
pub mod runtime;
pub mod workspace_copy;

/// Waits (bounded) until buffered stdout has been written. Binaries call this on every exit path.
pub fn flush_stdout_on_exit() -> std::io::Result<()> {
    output::drain_stdout(Some(std::time::Duration::from_secs(5)))
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

//! Dial's browser relay: lets the `browser` tool drive the user's own Chrome tabs through the Dial
//! Chrome extension, over a loopback port, without Chrome's remote-debugging flag.
//!
//! - `protocol`: wire types shared with the extension.
//! - `bridge`: relay state (tabs, sessions, clients). No sockets.
//! - `server`: the HTTP and WebSocket endpoints on one loopback port.
//! - `client`: the browser tool's side. Probes the relay, starts it if needed, waits for the extension.
//! - `install`: writes the extension folder with the relay port and token.

pub mod bridge;
pub mod client;
pub mod install;
pub mod outbox;
pub mod protocol;
pub mod server;

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;

use bridge::Relay;
use server::Shared;

/// Where the relay token lives under the Dial user directory.
pub fn token_path(user_dir: &Path) -> PathBuf {
    user_dir.join("browser-relay").join("token")
}

/// Returns the relay token, creating it on first use. Every relay and the extension share it.
/// Files and directories are owner-private on Unix and Windows; unsafe existing paths are refused.
pub fn load_or_create_token(user_dir: &Path) -> io::Result<String> {
    let path = token_path(user_dir);
    crate::platform::atomic::private_dir(path.parent().expect("token has a parent"))
        .map_err(private_io_error)?;
    if let Some(existing) =
        crate::platform::atomic::read_private(&path).map_err(private_io_error)?
    {
        if let Ok(existing) = std::str::from_utf8(&existing) {
            let token = existing.trim();
            if is_valid_token(token) {
                return Ok(token.to_owned());
            }
        }
    }
    let random: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let token = crate::hex(&random);
    write_private(&path, token.as_bytes())?;
    Ok(token)
}

/// Publishes relay secrets with owner-only protection established before writing any bytes.
pub(in crate::browser_relay) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::platform::atomic::write_private(path, bytes).map_err(private_io_error)
}

pub(in crate::browser_relay) fn private_io_error(error: anyhow::Error) -> io::Error {
    match error.downcast::<io::Error>() {
        Ok(error) => error,
        Err(error) => io::Error::other(error),
    }
}

fn is_valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Relays started in this process, by port. The browser tool starts at most one per port.
static IN_PROCESS_RELAYS: LazyLock<Mutex<HashMap<u16, Arc<Relay>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Starts a relay on loopback `port` inside this process, unless one already runs here.
/// Returns an error when another program holds the port.
pub fn ensure_serving(port: u16, user_dir: &Path) -> io::Result<()> {
    let mut relays = IN_PROCESS_RELAYS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if relays.contains_key(&port) {
        return Ok(());
    }
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    let relay = Arc::new(Relay::new());
    let token = load_or_create_token(user_dir)?;
    let shared = Arc::new(Shared {
        relay: Arc::clone(&relay),
        token,
        port: listener.local_addr()?.port(),
    });
    thread::Builder::new()
        .name("dial-browser-relay".to_owned())
        .spawn(move || server::serve(listener, shared))?;
    relays.insert(port, relay);
    Ok(())
}

/// Runs a relay in the foreground on `listener` until the process exits. The caller must bind
/// a loopback address; the relay refuses to serve anything else.
pub fn serve(listener: TcpListener, user_dir: &Path) -> io::Result<()> {
    let port = listener.local_addr()?.port();
    let shared = Arc::new(Shared {
        relay: Arc::new(Relay::new()),
        token: load_or_create_token(user_dir)?,
        port,
    });
    server::serve(listener, shared);
    Ok(())
}

#[cfg(test)]
mod tests;

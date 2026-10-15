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
/// The file is owner-only on unix; on Windows it inherits the per-user profile permissions.
pub fn load_or_create_token(user_dir: &Path) -> io::Result<String> {
    let path = token_path(user_dir);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let token = existing.trim();
        if is_valid_token(token) {
            return Ok(token.to_owned());
        }
    }
    let random: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
    let token = crate::hex(&random);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(&path, token.as_bytes())?;
    Ok(token)
}

/// Writes a file that holds relay secrets: replaced atomically, owner-only on unix.
pub(in crate::browser_relay) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::storage::atomic::write_replacing(path, bytes)?;
    restrict_to_owner(path)
}

fn is_valid_token(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> io::Result<()> {
    Ok(())
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
mod tests {
    use super::*;

    #[test]
    fn the_token_is_created_once_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_token(dir.path()).unwrap();
        assert!(is_valid_token(&first), "{first}");
        assert_eq!(load_or_create_token(dir.path()).unwrap(), first);
        assert_eq!(
            std::fs::read_to_string(token_path(dir.path())).unwrap(),
            first
        );
    }

    #[test]
    fn a_malformed_token_file_is_replaced_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(token_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(token_path(dir.path()), "short\n").unwrap();
        let token = load_or_create_token(dir.path()).unwrap();
        assert!(is_valid_token(&token));
        assert_ne!(token, "short");
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_create_token(dir.path()).unwrap();
        let mode = std::fs::metadata(token_path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

//! Writes the Dial Chrome extension into a folder the user loads with "Load unpacked".
//! The folder also carries `relay-config.js`, which tells the extension the relay port and token.

use std::io;
use std::path::{Path, PathBuf};

const FILES: [(&str, &str); 4] = [
    (
        "manifest.json",
        include_str!("../../assets/browser-extension/manifest.json"),
    ),
    (
        "background.js",
        include_str!("../../assets/browser-extension/background.js"),
    ),
    (
        "popup.html",
        include_str!("../../assets/browser-extension/popup.html"),
    ),
    (
        "popup.js",
        include_str!("../../assets/browser-extension/popup.js"),
    ),
];

/// Folder the extension is written to, under the Dial user directory.
pub fn extension_dir(user_dir: &Path) -> PathBuf {
    user_dir.join("browser-extension")
}

/// Writes the extension files and `relay-config.js` for a relay on `port`. Returns the folder.
pub fn install(user_dir: &Path, port: u16) -> io::Result<PathBuf> {
    let dir = extension_dir(user_dir);
    std::fs::create_dir_all(&dir)?;
    let token = super::load_or_create_token(user_dir)?;
    for (name, body) in FILES {
        crate::storage::atomic::write_replacing(&dir.join(name), body.as_bytes())?;
    }
    super::write_private(
        &dir.join("relay-config.js"),
        relay_config_js(port, &token).as_bytes(),
    )?;
    Ok(dir)
}

/// The `relay-config.js` body. `token` is hex, so it needs no escaping.
pub fn relay_config_js(port: u16, token: &str) -> String {
    format!("self.dialRelayConfig = {{\"port\":{port},\"token\":\"{token}\"}};\n")
}

#[cfg(test)]
mod tests;

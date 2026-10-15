//! `dial browser-relay`: write the Chrome extension, run the relay in the foreground, or check
//! whether the extension is connected. The browser tool starts the relay by itself, so `serve` is
//! only needed when the relay should outlive a Dial session or run on another port.

use std::net::{Ipv4Addr, SocketAddr, TcpListener};

use anyhow::{Context, Result};

use crate::browser_relay::{client, install, protocol::DEFAULT_PORT};
use crate::storage::paths::Layout;

/// Arguments for `dial browser-relay <action>`.
#[derive(Debug, Clone, clap::Args)]
pub struct BrowserRelayArgs {
    #[command(subcommand)]
    pub action: BrowserRelayAction,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum BrowserRelayAction {
    /// Write the Chrome extension to a folder and print how to load it.
    Install {
        /// Relay port the extension connects to. Use the port from browser.relay_url when it differs.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Run the relay in the foreground on 127.0.0.1. Press Ctrl-C to stop it.
    Serve {
        /// Loopback port to listen on.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Check whether the Chrome extension is connected to the relay.
    Status {
        /// Relay port to check.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
}

/// Runs the action and returns the process exit code: 0 on success, 1 when status finds no connection.
pub fn run(layout: &Layout, args: BrowserRelayArgs) -> Result<i32> {
    match args.action {
        BrowserRelayAction::Install { port } => {
            let folder = install::install(&layout.user_dir, port)
                .context("cannot write the Dial browser extension")?;
            println!(
                "Installed the Dial browser extension to:\n  {}",
                folder.display()
            );
            println!(
                "Load it once: open chrome://extensions, turn on Developer mode, choose \"Load unpacked\", and select that folder."
            );
            println!("It is configured for the relay on 127.0.0.1:{port}.");
            Ok(0)
        }
        BrowserRelayAction::Serve { port } => {
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .with_context(|| {
                    format!(
                        "cannot listen on 127.0.0.1:{port}; another program may be using the port"
                    )
                })?;
            println!("Dial browser relay listening on 127.0.0.1:{port}. Press Ctrl-C to stop.");
            crate::browser_relay::serve(listener, &layout.user_dir)
                .context("the browser relay stopped with an error")?;
            Ok(0)
        }
        BrowserRelayAction::Status { port } => {
            match client::status_report(port, &layout.user_dir) {
                Ok(line) => {
                    println!("{line}");
                    Ok(0)
                }
                Err(reason) => {
                    eprintln!("{reason}");
                    Ok(1)
                }
            }
        }
    }
}

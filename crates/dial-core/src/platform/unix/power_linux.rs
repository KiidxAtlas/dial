use std::os::fd::OwnedFd;
use std::time::Duration;

use anyhow::{Context, Result};

#[derive(Debug)]
pub(super) struct Request {
    // logind keeps the inhibitor until this descriptor closes, independently of
    // the D-Bus connection and of any other inhibitor held by this process.
    _inhibitor: OwnedFd,
}

impl Request {
    pub(super) fn acquire() -> Result<Self> {
        let connection = zbus::blocking::connection::Builder::system()
            .context("idle sleep prevention requires an accessible system D-Bus")?
            .method_timeout(Duration::from_secs(3))
            .build()
            .context("connecting to system D-Bus for idle sleep prevention; check that the system bus is running")?;
        // 'idle', not 'sleep': inhibit logind's automatic idle handling without
        // blocking explicit suspend requests. Desktops with their own idle policy
        // may not honor logind idle inhibitors.
        let reply = connection
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "Inhibit",
                &("idle", "Dial", "Dial active agent work", "block"),
            )
            .context("logind idle inhibition failed; ensure systemd-logind is running and policy permits idle inhibitors")?;
        let descriptor: zbus::zvariant::OwnedFd = reply
            .body()
            .deserialize()
            .context("logind returned an invalid idle inhibitor descriptor")?;
        Ok(Self {
            _inhibitor: descriptor.into(),
        })
    }
}

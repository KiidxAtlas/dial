//! Scoped prevention of automatic idle system sleep, without keeping the display awake.

#[cfg(target_os = "macos")]
#[path = "unix/power_macos.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "unix/power_linux.rs"]
mod imp;
#[cfg(windows)]
#[path = "windows/power.rs"]
mod imp;

use std::sync::atomic::{AtomicBool, Ordering};

/// An independent OS request to prevent idle system sleep until this guard is dropped.
///
/// Does not disable screen locking, display sleep, or explicit user suspend. OS power
/// policy (including critical battery and lid-close behavior) can override requests.
/// Each guard owns its own request; dropping one does not release another's request.
#[derive(Debug)]
#[must_use = "dropping the guard immediately releases sleep prevention"]
pub struct IdleSleepGuard {
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    _request: imp::Request,
}

impl IdleSleepGuard {
    /// Ask the operating system to prevent idle system sleep while Dial is working.
    ///
    /// # Errors
    /// Returns an error when the native request fails or the platform is unsupported.
    /// Linux requires a system D-Bus and logind with permission to inhibit idle handling.
    /// This is a blocking operation; Linux method calls have a three-second timeout.
    pub fn acquire() -> anyhow::Result<Self> {
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        {
            Ok(Self {
                _request: imp::Request::acquire()?,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        anyhow::bail!("idle sleep prevention is unsupported on this operating system")
    }

    /// [`acquire`](Self::acquire), reporting a failure only the first time in this process.
    ///
    /// Every call still asks the OS, so a mechanism that becomes available is used. A missing
    /// mechanism (a container without a system bus) is reported once instead of on every turn.
    ///
    /// # Errors
    /// Returns the first failure in this process; later failures yield `Ok(None)`.
    pub fn acquire_reporting_once() -> anyhow::Result<Option<Self>> {
        static REPORTED: AtomicBool = AtomicBool::new(false);
        report_once(Self::acquire(), &REPORTED)
    }
}

/// `Ok(Some)` on success; the first error is returned and marks `reported`, later errors are `Ok(None)`.
fn report_once<T>(result: anyhow::Result<T>, reported: &AtomicBool) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if !reported.swap(true, Ordering::Relaxed) => Err(error),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests;

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
}

#[cfg(test)]
mod tests;

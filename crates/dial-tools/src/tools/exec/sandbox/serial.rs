//! Opt-in access to USB serial adapters for sandboxed commands (`sandbox.serial_ports`), so build
//! tools such as `arduino-cli upload`, `esptool` and PlatformIO can flash boards and read their
//! serial output while the rest of the sandbox stays in force.
//!
//! Only USB serial adapters are granted, never other devices or terminals:
//! * macOS: `/dev/cu.*` and `/dev/tty.*` named `usbserial…` (FTDI, CH340, CP210x), `usbmodem…`
//!   (CDC ACM: Arduino Uno R3/Leonardo, native USB on ESP32-S2/S3/C3), `SLAB_USBtoUART…` (older
//!   CP210x driver) or `wchusbserial…` (older CH340 driver). Bluetooth and debug ports stay closed.
//!   The grant is a name pattern, so a board plugged in after Dial starts is covered.
//! * Linux/WSL2: `/dev/ttyUSB<n>` and `/dev/ttyACM<n>` present when the command starts are bound
//!   into its private `/dev`; a board plugged in later is seen by the next command. The user still
//!   needs the usual device permission (for example the `dialout` group).
//! * Windows: COM ports are not file paths; the setting changes nothing there.

/// Seatbelt rule appended to a command's profile; `file-ioctl` lets tools set the baud rate and
/// toggle DTR/RTS to reset a board into its bootloader.
#[cfg(any(all(unix, not(target_os = "linux")), test))]
pub(super) const MACOS_RULE: &str = "(allow file-read* file-write* file-ioctl (regex #\"^/dev/(cu|tty)\\.(usbserial|usbmodem|SLAB_USBtoUART|wchusbserial)[^/]*$\"))\n";

/// Whether a `/dev` entry name is a Linux USB serial adapter: `ttyUSB<n>` or `ttyACM<n>`.
#[cfg(any(target_os = "linux", test))]
pub(super) fn is_linux_serial(name: &str) -> bool {
    ["ttyUSB", "ttyACM"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

/// The USB serial adapters present now, sorted.
#[cfg(any(target_os = "linux", all(test, unix)))]
pub(super) fn linux_devices() -> Vec<std::path::PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_str().is_some_and(is_linux_serial))
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests;

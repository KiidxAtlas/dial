use super::*;

#[test]
fn linux_names_cover_usb_adapters_only() {
    for name in ["ttyUSB0", "ttyUSB12", "ttyACM0", "ttyACM3"] {
        assert!(is_linux_serial(name), "{name}");
    }
    for name in [
        "ttyUSB", "ttyACM", "ttyS0", "tty1", "tty", "ttyUSB0x", "ptmx", "null", "sda",
    ] {
        assert!(!is_linux_serial(name), "{name}");
    }
}

/// The SBPL regex, checked with the same pattern in Rust (Seatbelt regexes are POSIX ERE).
#[test]
fn macos_rule_matches_usb_serial_ports_and_nothing_else() {
    let start = MACOS_RULE.find("#\"").unwrap() + 2;
    let end = MACOS_RULE[start..].find('"').unwrap() + start;
    let pattern = regex::Regex::new(&MACOS_RULE[start..end]).unwrap();
    for name in [
        "/dev/cu.usbserial-0001",
        "/dev/tty.usbserial-A50285BI",
        "/dev/cu.usbmodem14101",
        "/dev/cu.SLAB_USBtoUART",
        "/dev/cu.wchusbserial1410",
    ] {
        assert!(pattern.is_match(name), "{name}");
    }
    for name in [
        "/dev/cu.Bluetooth-Incoming-Port",
        "/dev/cu.debug-console",
        "/dev/ttys001",
        "/dev/tty",
        "/dev/disk0",
        "/dev/cu.usbserial-1/../disk0",
        "/private/dev/cu.usbserial-1",
    ] {
        assert!(!pattern.is_match(name), "{name}");
    }
    assert!(MACOS_RULE.contains("file-ioctl"));
}

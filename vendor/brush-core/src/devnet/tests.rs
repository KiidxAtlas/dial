use std::path::Path;

use super::{Protocol, Target, parse, try_open};

/// Parses `path` into `Ok(protocol, host, port)` or `Err(message)`. `None` means not a
/// network target.
fn parsed(path: &str) -> Option<Result<(Protocol, String, u16), String>> {
    parse(path).map(|r| {
        r.map(
            |Target {
                 protocol,
                 host,
                 port,
             }| (protocol, host.to_string(), port),
        )
        .map_err(|e| e.to_string())
    })
}

#[test]
fn network_targets_split_host_and_numeric_port() {
    let cases = [
        ("/dev/tcp/127.0.0.1/8080", Protocol::Tcp, "127.0.0.1", 8080),
        (
            "/dev/tcp/example.test/80",
            Protocol::Tcp,
            "example.test",
            80,
        ),
        ("/dev/tcp/::1/22", Protocol::Tcp, "::1", 22),
        ("/dev/tcp/[::1]/22", Protocol::Tcp, "::1", 22),
        ("/dev/udp/127.0.0.1/53", Protocol::Udp, "127.0.0.1", 53),
    ];
    for (path, protocol, host, port) in cases {
        assert_eq!(
            parsed(path),
            Some(Ok((protocol, host.to_string(), port))),
            "{path}"
        );
    }
}

#[test]
fn malformed_network_targets_are_not_found() {
    for path in [
        "/dev/tcp/localhost",
        "/dev/tcp/localhost/",
        "/dev/tcp//80",
        "/dev/tcp/host/http",
        "/dev/tcp/host/70000",
    ] {
        assert_eq!(
            parsed(path),
            Some(Err("No such file or directory".to_string())),
            "{path}"
        );
    }
}

#[test]
fn ordinary_paths_are_left_to_the_filesystem() {
    for path in [
        "/dev/null",
        "/tmp/dev/tcp/1/2",
        "dev/tcp/1/2",
        "/dev/tcpx/1/2",
    ] {
        assert!(parsed(path).is_none(), "{path}");
        assert!(try_open(Path::new(path)).is_none(), "{path}");
    }
}

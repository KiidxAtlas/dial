use super::*;

fn rules(allow: &[&str], deny: &[&str]) -> NetworkRules {
    let owned = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    NetworkRules::new(&owned(allow), &owned(deny))
        .unwrap()
        .expect("non-empty rules")
}

#[test]
fn empty_lists_mean_unfiltered() {
    assert_eq!(NetworkRules::new(&[], &[]).unwrap(), None);
}

#[test]
fn patterns_are_validated_and_normalized() {
    let parsed = rules(
        &["Example.COM.", "*.crates.io", "10.0.0.5", "[::1]", "*"],
        &[],
    );
    assert_eq!(
        parsed.allow,
        vec!["example.com", "*.crates.io", "10.0.0.5", "::1", "*"]
    );
    for bad in [
        "https://example.com",
        "example.com:443",
        "example.com/path",
        "",
        "*.",
        "-bad.com",
        "a..b",
        "**.example.com",
    ] {
        let error = NetworkRules::new(&[bad.to_string()], &[]).unwrap_err();
        assert!(error.contains("sandbox.network_allow"), "{bad}: {error}");
    }
    assert!(
        NetworkRules::new(&[], &["bad host".to_string()])
            .unwrap_err()
            .contains("sandbox.network_deny")
    );
}

#[test]
fn allow_list_exact_wildcard_and_deny_precedence() {
    let rules = rules(
        &["github.com", "*.githubusercontent.com"],
        &["gist.githubusercontent.com"],
    );
    assert_eq!(
        rules.decide("GitHub.com."),
        Decision::Allow { private_ok: true }
    );
    assert_eq!(
        rules.decide("raw.githubusercontent.com"),
        Decision::Allow { private_ok: false }
    );
    // A wildcard covers subdomains only, not the apex or look-alike suffixes.
    assert_eq!(rules.decide("githubusercontent.com"), Decision::Deny);
    assert_eq!(rules.decide("evilgithubusercontent.com"), Decision::Deny);
    assert_eq!(rules.decide("api.github.com"), Decision::Deny);
    assert_eq!(rules.decide("gist.githubusercontent.com"), Decision::Deny);
    assert_eq!(rules.decide("example.org"), Decision::Deny);
}

#[test]
fn deny_only_rules_allow_everything_else_without_private_access() {
    let rules = rules(&[], &["*.tracker.example", "pastebin.com"]);
    assert_eq!(
        rules.decide("crates.io"),
        Decision::Allow { private_ok: false }
    );
    assert_eq!(rules.decide("a.tracker.example"), Decision::Deny);
    assert_eq!(rules.decide("pastebin.com"), Decision::Deny);
}

#[test]
fn non_public_addresses_are_recognized() {
    for private in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fd00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "64:ff9b::a00:1",
        "2001:db8::1",
        "2002:7f00:1::",
        "2002:a00:1::1",
    ] {
        assert!(!is_public(private.parse().unwrap()), "{private}");
    }
    for public in [
        "1.1.1.1",
        "140.82.112.3",
        "2606:4700:4700::1111",
        "64:ff9b::101:101",
        "2002:101:101::1",
    ] {
        assert!(is_public(public.parse().unwrap()), "{public}");
    }
}

#[test]
fn client_env_points_every_standard_variable_at_the_proxy() {
    let env = client_env(4242);
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "https_proxy"] {
        assert!(
            env.contains(&(key, "http://127.0.0.1:4242".to_string())),
            "{key}"
        );
    }
    assert!(env.contains(&("NO_PROXY", String::new())));
    assert!(env.contains(&("no_proxy", String::new())));
}

#[cfg(unix)]
mod live {
    use super::super::server::{handle, parse, spawn_bridge};
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::net::UnixListener;

    #[test]
    fn parse_connect_and_absolute_http() {
        let connect =
            parse(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\n\r\n").unwrap();
        assert_eq!((connect.host.as_str(), connect.port), ("example.com", 443));
        assert!(connect.forward.is_none());
        let v6 = parse(b"CONNECT [2001:db8::1]:8443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("2001:db8::1", 8443));
        let http = parse(
            b"GET http://user:pw@Example.com:8080/a?b=1 HTTP/1.1\r\nHost: example.com:8080\r\nProxy-Authorization: x\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        assert_eq!((http.host.as_str(), http.port), ("Example.com", 8080));
        let forward = String::from_utf8(http.forward.unwrap()).unwrap();
        assert_eq!(
            forward,
            "GET /a?b=1 HTTP/1.1\r\nHost: example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
        let bare = parse(b"GET http://example.com HTTP/1.0\r\n\r\n").unwrap();
        let forward = String::from_utf8(bare.forward.unwrap()).unwrap();
        assert!(
            forward.starts_with("GET / HTTP/1.0\r\nHost: example.com\r\n"),
            "{forward}"
        );
        assert_eq!(bare.port, 80);
        for bad in [
            &b"GET /relative HTTP/1.1\r\n\r\n"[..],
            b"GET https://example.com/ HTTP/1.1\r\n\r\n",
            b"CONNECT example.com HTTP/1.1\r\n\r\n",
            b"CONNECT example.com:0 HTTP/1.1\r\n\r\n",
            b"CONNECT example.com:443 HTTP/2\r\n\r\n",
            b"garbage\r\n\r\n",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// Run one proxy exchange over a socket pair and return the client's view of it.
    fn exchange(rules: &NetworkRules, request: &[u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let rules = rules.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = handle(stream, &rules);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client.write_all(request).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).unwrap();
        server.join().unwrap();
        reply
    }

    #[test]
    fn denied_hosts_get_403_without_any_connection() {
        let rules = rules(&["allowed.example"], &[]);
        let reply = exchange(&rules, b"CONNECT blocked.example:443 HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{reply}");
        assert!(reply.contains("X-Dial-Sandbox: network-policy"));
        assert!(reply.contains("does not allow blocked.example"));
    }

    #[test]
    fn wildcard_allowed_host_resolving_to_loopback_is_blocked() {
        // `localhost` resolves to loopback; only an exact entry may reach non-public addresses.
        let rules = rules(&["*"], &[]);
        let reply = exchange(&rules, b"CONNECT localhost:9 HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(reply.contains("non-public address"), "{reply}");
    }

    #[test]
    fn malformed_requests_get_400() {
        let rules = rules(&["*"], &[]);
        let reply = exchange(&rules, b"GET /relative HTTP/1.1\r\n\r\n");
        assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    }

    /// An upstream that answers one request with a fixed body and records what it received.
    fn upstream() -> (u16, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            let mut chunk = [0u8; 1024];
            while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                )
                .unwrap();
            received
        });
        (port, thread)
    }

    #[test]
    fn exact_allow_entry_reaches_private_upstream_through_connect_and_http() {
        let rules = rules(&["127.0.0.1"], &[]);
        let (port, server) = upstream();
        let reply = exchange(
            &rules,
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n")
                .as_bytes(),
        );
        assert!(
            reply.starts_with("HTTP/1.1 200 Connection Established\r\n\r\nHTTP/1.1 200 OK"),
            "{reply}"
        );
        assert!(reply.ends_with("hello"));
        assert!(
            String::from_utf8(server.join().unwrap())
                .unwrap()
                .starts_with("GET / HTTP/1.1")
        );

        let (port, server) = upstream();
        let reply = exchange(
            &rules,
            format!("GET http://127.0.0.1:{port}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
        );
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        let received = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(received.starts_with("GET /x HTTP/1.1\r\n"), "{received}");
        assert!(received.contains("Connection: close\r\n"));
    }

    #[test]
    fn shared_endpoint_serves_tcp_and_unix_clients() {
        let rules = rules(&["only.example"], &["*.other.example"]);
        let endpoint = endpoint(&rules).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &endpoint,
            &super::super::endpoint(&rules).unwrap()
        ));
        let request = b"CONNECT denied.example:443 HTTP/1.1\r\n\r\n";
        let mut tcp = TcpStream::connect(("127.0.0.1", endpoint.port())).unwrap();
        tcp.write_all(request).unwrap();
        let mut reply = String::new();
        tcp.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        let mut unix = std::os::unix::net::UnixStream::connect(endpoint.socket()).unwrap();
        unix.write_all(request).unwrap();
        let mut reply = String::new();
        unix.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    }

    #[test]
    fn bridge_splices_loopback_connections_to_the_socket() {
        let dir = tempfile::Builder::new()
            .prefix("dial-bridge-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("s.sock");
        let unix = UnixListener::bind(&socket).unwrap();
        let echo = std::thread::spawn(move || {
            let (mut stream, _) = unix.accept().unwrap();
            let mut buffer = [0u8; 4];
            stream.read_exact(&mut buffer).unwrap();
            stream.write_all(&buffer).unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        spawn_bridge(listener, socket).unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(b"ping").unwrap();
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"ping");
        echo.join().unwrap();
    }
}

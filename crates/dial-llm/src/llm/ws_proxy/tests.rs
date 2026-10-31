use super::*;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;

fn unbounded() -> Result<Option<Duration>> {
    Ok(Some(Duration::from_secs(5)))
}

/// A one-shot CONNECT proxy: returns the request head it received; answers `status`, then on
/// success writes `tunnelled` as if from the target.
fn proxy(status: &'static str) -> (u16, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end().to_owned();
            if line.is_empty() {
                break;
            }
            head.push(line);
        }
        let mut stream = stream;
        stream
            .write_all(format!("HTTP/1.1 {status}\r\nX-Proxy: test\r\n\r\n").as_bytes())
            .unwrap();
        if status.starts_with('2') {
            stream.write_all(b"tunnelled").unwrap();
        }
        head
    });
    (port, handle)
}

#[test]
fn a_connect_proxy_tunnels_to_the_target_with_its_credentials() {
    let (port, server) = proxy("200 Connection established");
    let proxy = ureq::Proxy::new(&format!("http://user:secret@127.0.0.1:{port}")).unwrap();
    let mut tcp = open_with(Some(&proxy), "chatgpt.example", 443, &unbounded).unwrap();
    let mut tunnelled = [0u8; 9];
    tcp.read_exact(&mut tunnelled).unwrap();
    assert_eq!(
        &tunnelled, b"tunnelled",
        "no tunnelled byte may be consumed"
    );
    let head = server.join().unwrap();
    assert_eq!(head[0], "CONNECT chatgpt.example:443 HTTP/1.1");
    assert!(head.contains(&"Host: chatgpt.example:443".to_owned()));
    let expected = base64::engine::general_purpose::STANDARD.encode("user:secret");
    assert!(head.contains(&format!("Proxy-Authorization: Basic {expected}")));
}

#[test]
fn ipv6_targets_are_bracketed_in_the_connect_request() {
    let (port, server) = proxy("200 OK");
    let proxy = ureq::Proxy::new(&format!("http://127.0.0.1:{port}")).unwrap();
    open_with(Some(&proxy), "2001:db8::1", 443, &unbounded).unwrap();
    let head = server.join().unwrap();
    assert_eq!(head[0], "CONNECT [2001:db8::1]:443 HTTP/1.1");
    assert!(
        !head
            .iter()
            .any(|line| line.starts_with("Proxy-Authorization"))
    );
}

#[test]
fn a_refused_tunnel_fails_with_the_proxy_status() {
    let (port, server) = proxy("407 Proxy Authentication Required");
    let proxy = ureq::Proxy::new(&format!("http://127.0.0.1:{port}")).unwrap();
    let error = open_with(Some(&proxy), "chatgpt.example", 443, &unbounded).unwrap_err();
    server.join().unwrap();
    assert!(
        error
            .to_string()
            .contains("HTTP/1.1 407 Proxy Authentication Required"),
        "{error:#}"
    );
}

#[test]
fn unsupported_proxy_kinds_fail_before_connecting() {
    let proxy = ureq::Proxy::new("socks5://127.0.0.1:1").unwrap();
    let error = open_with(Some(&proxy), "chatgpt.example", 443, &unbounded).unwrap_err();
    assert!(error.to_string().contains("CONNECT proxies"), "{error:#}");
}

#[test]
fn local_hosts_never_use_a_proxy() {
    assert!(proxy_for("127.0.0.1").is_none());
    assert!(proxy_for("localhost").is_none());
}

#[test]
fn websocket_sockets_enable_tcp_keepalive_direct_and_through_a_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || listener.accept().unwrap());
    let direct = open_with(None, "127.0.0.1", port, &unbounded).unwrap();
    assert!(
        socket2::SockRef::from(&direct).keepalive().unwrap(),
        "a direct WebSocket socket must send keepalive probes"
    );
    server.join().unwrap();

    let (proxy_port, server) = proxy("200 Connection established");
    let proxy = ureq::Proxy::new(&format!("http://127.0.0.1:{proxy_port}")).unwrap();
    let tunnel = open_with(Some(&proxy), "chatgpt.example", 443, &unbounded).unwrap();
    assert!(
        socket2::SockRef::from(&tunnel).keepalive().unwrap(),
        "the proxy tunnel's socket must send keepalive probes"
    );
    server.join().unwrap();
}

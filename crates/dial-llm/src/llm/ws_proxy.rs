//! TCP connections for WebSocket upgrades. HTTP requests go through ureq, which honours the
//! environment's proxy settings; a WebSocket opens its own socket, so it applies the same
//! `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY` and `NO_PROXY` settings here with an HTTP CONNECT tunnel.
//! Loopback and LAN hosts are always reached directly, as for HTTP.

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Longest proxy response head accepted for a CONNECT request.
const MAX_CONNECT_HEAD: usize = 8 * 1024;

/// Time left for connection setup; `None` is unbounded. Errors end the setup (deadline or
/// cancellation).
pub(crate) type Remaining<'a> = &'a dyn Fn() -> Result<Option<Duration>>;

/// Opens a TCP connection to `host:port`, tunnelled through the environment's proxy when one
/// applies to `host`. TLS and the WebSocket handshake run on the returned stream.
pub(crate) fn open(host: &str, port: u16, remaining: Remaining<'_>) -> Result<TcpStream> {
    open_with(proxy_for(host).as_ref(), host, port, remaining)
}

/// The environment proxy for `host`, unless it is local or excluded by `NO_PROXY`.
fn proxy_for(host: &str) -> Option<ureq::Proxy> {
    if super::is_local_host(host) {
        return None;
    }
    let proxy = ureq::Proxy::try_from_env()?;
    let uri = format!("https://{}/", authority_host(host))
        .parse::<ureq::http::Uri>()
        .ok()?;
    (!proxy.is_no_proxy(&uri)).then_some(proxy)
}

fn open_with(
    proxy: Option<&ureq::Proxy>,
    host: &str,
    port: u16,
    remaining: Remaining<'_>,
) -> Result<TcpStream> {
    let Some(proxy) = proxy else {
        return connect_any(host, port, remaining);
    };
    ensure!(
        proxy.protocol() == ureq::ProxyProtocol::Http,
        "WebSocket connections support only http:// CONNECT proxies (the configured proxy uses {:?})",
        proxy.protocol()
    );
    let mut tcp = connect_any(proxy.host(), proxy.port(), remaining)
        .context("connecting to the HTTP proxy")?;
    let target = format!("{}:{port}", authority_host(host));
    let mut head = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if proxy.username().is_some() || proxy.password().is_some() {
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            proxy.username().unwrap_or_default(),
            proxy.password().unwrap_or_default()
        ));
        head.push_str(&format!("Proxy-Authorization: Basic {credentials}\r\n"));
    }
    head.push_str("\r\n");
    tcp.set_write_timeout(remaining()?)?;
    tcp.write_all(head.as_bytes())
        .context("sending the proxy CONNECT request")?;
    // Byte by byte: after its head the proxy sends nothing until the client starts TLS, so no
    // tunnelled byte is consumed here.
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        ensure!(
            response.len() < MAX_CONNECT_HEAD,
            "HTTP proxy CONNECT response head is too large"
        );
        tcp.set_read_timeout(remaining()?)?;
        let read = tcp
            .read(&mut byte)
            .context("reading the proxy CONNECT response")?;
        if read == 0 {
            bail!("HTTP proxy closed the connection during CONNECT");
        }
        response.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&response);
    let status_line = head.lines().next().unwrap_or_default().trim();
    let status = status_line.split_whitespace().nth(1).unwrap_or_default();
    ensure!(
        status.starts_with('2') && status.len() == 3,
        "HTTP proxy refused the WebSocket tunnel (response {status_line:?})"
    );
    tcp.set_read_timeout(None)?;
    tcp.set_write_timeout(None)?;
    Ok(tcp)
}

/// Connects to the first reachable address of `host:port` within the remaining budget. Each
/// attempt gets an even share of the time left, so an unreachable first address (a blackholed
/// IPv6 route, say) cannot consume the whole budget before a reachable one is tried.
fn connect_any(host: &str, port: u16, remaining: Remaining<'_>) -> Result<TcpStream> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses: Vec<_> = (host, port).to_socket_addrs()?.collect();
    remaining()?;
    let mut last = None;
    for (index, addr) in addresses.iter().enumerate() {
        let attempt = match remaining()? {
            Some(left) => {
                let share = left / u32::try_from(addresses.len() - index).unwrap_or(u32::MAX);
                TcpStream::connect_timeout(addr, share.max(Duration::from_millis(1)))
            }
            None => TcpStream::connect(addr),
        };
        remaining()?;
        match attempt {
            Ok(stream) => {
                super::cancel::keepalive(&stream)?;
                return Ok(stream);
            }
            Err(error) => last = Some(error),
        }
    }
    match last {
        Some(error) => Err(error.into()),
        None => bail!("{host} resolved to no addresses"),
    }
}

/// `host` as it appears in an authority: IPv6 literals in brackets.
fn authority_host(host: &str) -> String {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.contains(':') {
        format!("[{bare}]")
    } else {
        bare.to_owned()
    }
}

#[cfg(test)]
mod tests;

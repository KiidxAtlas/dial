//! Bash's network redirection targets: `/dev/tcp/HOST/PORT` and `/dev/udp/HOST/PORT`.
//!
//! Opening one of these paths connects a socket instead of opening a file. The socket is a
//! bidirectional descriptor, so it serves `<`, `>`, `>>` and `<>` alike, like bash.
//!
//! * Unix: the connected socket's descriptor becomes an [`OpenFile::File`]. Builtins and external
//!   children both read and write it directly, so nothing else is needed.
//! * Windows: sockets are not file handles. The socket is wrapped in an [`OpenFile::Stream`].
//!   Builtins read and write the socket directly. When the descriptor is handed to an external
//!   child, [`crate::openfiles::Stream::child_stdio`] bridges it through an anonymous pipe with a
//!   relay thread for that one direction.
//!
//! No connect timeout is applied, as in bash. A blocked connect or read is stopped only when the
//! shell run is aborted.

use std::io;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};

use crate::openfiles::OpenFile;

const TCP_PREFIX: &str = "/dev/tcp/";
const UDP_PREFIX: &str = "/dev/udp/";

/// Transport selected by a network redirection path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Tcp,
    Udp,
}

/// A parsed `/dev/tcp/HOST/PORT` or `/dev/udp/HOST/PORT` path.
#[derive(Debug, PartialEq, Eq)]
struct Target<'a> {
    protocol: Protocol,
    host: &'a str,
    port: u16,
}

/// A connected socket, before it is turned into an [`OpenFile`].
enum Endpoint {
    Tcp(TcpStream),
    Udp(UdpSocket),
}

/// Opens `path` if it names a network redirection target.
///
/// Returns `None` for every other path, so ordinary files are unaffected. A path that names a
/// target but is malformed or fails to connect returns `Some(Err(..))`, which the caller reports
/// as a redirection failure.
pub(crate) fn try_open(path: &std::path::Path) -> Option<io::Result<OpenFile>> {
    let path = path.to_str()?;
    let target = match parse(path)? {
        Ok(target) => target,
        Err(err) => return Some(Err(err)),
    };
    Some(connect(&target).map(into_open_file))
}

/// Parses a network redirection path. Returns `None` if `path` is not one.
fn parse(path: &str) -> Option<io::Result<Target<'_>>> {
    let (protocol, rest) = if let Some(rest) = path.strip_prefix(TCP_PREFIX) {
        (Protocol::Tcp, rest)
    } else {
        (Protocol::Udp, path.strip_prefix(UDP_PREFIX)?)
    };

    Some(parse_host_and_port(protocol, rest))
}

/// Splits `HOST/PORT`. Like bash, the host runs to the first `/`, and an IPv6 literal may be
/// written bare (`::1`) or in brackets (`[::1]`).
fn parse_host_and_port(protocol: Protocol, rest: &str) -> io::Result<Target<'_>> {
    let not_found = || io::Error::new(io::ErrorKind::NotFound, "No such file or directory");

    let (host, port) = rest.split_once('/').ok_or_else(not_found)?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        return Err(not_found());
    }
    // Service names (for example `http`) are not resolved; only numeric ports are accepted.
    let port: u16 = port.parse().map_err(|_| not_found())?;

    Ok(Target {
        protocol,
        host,
        port,
    })
}

/// Connects to the target, trying each resolved address in turn as bash does.
fn connect(target: &Target<'_>) -> io::Result<Endpoint> {
    let addrs: Vec<SocketAddr> = (target.host, target.port)
        .to_socket_addrs()
        .map_err(|_| lookup_failure())?
        .collect();
    if addrs.is_empty() {
        return Err(lookup_failure());
    }

    let mut last_error = None;
    for addr in addrs {
        let attempt = match target.protocol {
            Protocol::Tcp => TcpStream::connect(addr).map(Endpoint::Tcp),
            Protocol::Udp => connect_udp(addr).map(Endpoint::Udp),
        };
        match attempt {
            Ok(endpoint) => return Ok(endpoint),
            Err(err) => last_error = Some(err),
        }
    }

    // `addrs` is non-empty, so at least one attempt set `last_error`.
    Err(last_error.unwrap_or_else(lookup_failure))
}

/// Binds an ephemeral local socket of the same family as `addr` and connects it to `addr`.
fn connect_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let local = if addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(local)?;
    socket.connect(addr)?;
    Ok(socket)
}

fn lookup_failure() -> io::Error {
    io::Error::other("Name or service not known")
}

/// Turns a connected socket into the shell's open file for it.
fn into_open_file(endpoint: Endpoint) -> OpenFile {
    #[cfg(unix)]
    {
        let fd = match endpoint {
            Endpoint::Tcp(stream) => std::os::fd::OwnedFd::from(stream),
            Endpoint::Udp(socket) => std::os::fd::OwnedFd::from(socket),
        };
        OpenFile::File(std::fs::File::from(fd))
    }

    #[cfg(windows)]
    {
        OpenFile::Stream(Box::new(windows::SocketStream::new(endpoint)))
    }
}

#[cfg(windows)]
mod windows {
    use std::io::Write as _;
    use std::net::TcpStream;
    use std::process::Stdio;
    use std::sync::Arc;

    use super::Endpoint;
    use crate::openfiles::{ChildStreamDirection, Stream};

    /// Size of each relay read.
    const RELAY_CHUNK: usize = 8192;

    /// Shell-side handle to a connected socket on Windows.
    ///
    /// Clones share the socket, so `>&3` and subshells see the same connection.
    pub(super) struct SocketStream {
        endpoint: Arc<Endpoint>,
    }

    impl SocketStream {
        pub(super) fn new(endpoint: Endpoint) -> Self {
            Self {
                endpoint: Arc::new(endpoint),
            }
        }
    }

    impl std::io::Read for SocketStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.endpoint.read(buf)
        }
    }

    impl std::io::Write for SocketStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.endpoint.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Stream for SocketStream {
        fn clone_box(&self) -> Box<dyn Stream> {
            Box::new(Self {
                endpoint: Arc::clone(&self.endpoint),
            })
        }

        fn child_stdio(&self, direction: ChildStreamDirection) -> Stdio {
            // A relay that cannot start leaves the child with no usable stream, so it reads or
            // writes nothing, the same as `/dev/null`.
            relay(Arc::clone(&self.endpoint), direction).unwrap_or_else(|_| Stdio::null())
        }
    }

    impl Endpoint {
        pub(super) fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self {
                Self::Tcp(stream) => {
                    let mut stream: &TcpStream = stream;
                    std::io::Read::read(&mut stream, buf)
                }
                Self::Udp(socket) => socket.recv(buf),
            }
        }

        pub(super) fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                Self::Tcp(stream) => {
                    let mut stream: &TcpStream = stream;
                    std::io::Write::write(&mut stream, buf)
                }
                Self::Udp(socket) => socket.send(buf),
            }
        }

        fn write_all(&self, mut buf: &[u8]) -> std::io::Result<()> {
            while !buf.is_empty() {
                match self.write(buf)? {
                    0 => return Err(std::io::ErrorKind::WriteZero.into()),
                    n => buf = &buf[n..],
                }
            }
            Ok(())
        }
    }

    /// Bridges one direction of the socket to an anonymous pipe that the child uses.
    ///
    /// `Input` copies socket data into the pipe the child reads from. `Output` copies the pipe
    /// the child writes to into the socket. Each relay ends when its pipe end closes, and bytes
    /// the child never reads are lost with the pipe.
    fn relay(endpoint: Arc<Endpoint>, direction: ChildStreamDirection) -> std::io::Result<Stdio> {
        match direction {
            ChildStreamDirection::Input => {
                let (reader, mut writer) = std::io::pipe()?;
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; RELAY_CHUNK];
                    loop {
                        match endpoint.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if writer.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                Ok(Stdio::from(reader))
            }
            ChildStreamDirection::Output => {
                let (mut reader, writer) = std::io::pipe()?;
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; RELAY_CHUNK];
                    loop {
                        match std::io::Read::read(&mut reader, &mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if endpoint.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                Ok(Stdio::from(writer))
            }
        }
    }
}

#[cfg(test)]
mod tests;

//! Lifetime of the scripted loopback servers used by the image transport fixtures.
use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

/// A scripted loopback server thread. [`Server::join`] stops it as soon as the client is done
/// instead of letting it wait out an idle period, which held every test open for seconds.
pub(super) struct Server {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Vec<String>>,
}

impl Server {
    /// Runs `serve` on its own thread. `serve` returns the requests it received once the flag is
    /// set and no connection is pending, or at its own safety bound if the server is never joined.
    pub(super) fn spawn(serve: impl FnOnce(&AtomicBool) -> Vec<String> + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        Self {
            stop,
            handle: std::thread::spawn(move || serve(&flag)),
        }
    }

    /// Call once the client has returned: no request is in flight, so the server stops after
    /// accepting anything already queued. Returns every request received.
    pub(super) fn join(self) -> std::thread::Result<Vec<String>> {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join()
    }
}

/// Reads one request head and its body from a loopback connection, as raw text.
pub(super) fn read_request(stream: &TcpStream) -> String {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut raw = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap();
        }
        let end = line == "\r\n" || line.is_empty();
        raw.push_str(&line);
        if end {
            break;
        }
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    raw.push_str(&String::from_utf8_lossy(&body));
    raw
}

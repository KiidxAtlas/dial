//! Bounded SFTP v3 transfers for hosts without a POSIX shell. No remote command interpolation.
use super::{Settings, Target, connection, url::RemotePath};
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const LIMIT: usize = 1024 * 1024;
const CHUNK: usize = 32 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// A binary string, not shell or batch-file syntax.
fn string(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

fn remote_path(path: &RemotePath) -> String {
    match path {
        RemotePath::Absolute(path) => path.clone(),
        RemotePath::Home(path) if path.is_empty() => ".".into(),
        // SFTP starts in the authenticated user's home directory. Prefixing ./ prevents
        // server-side tilde expansion from interpreting a literal first component.
        RemotePath::Home(path) => format!("./{path}"),
    }
}

struct Client<R, W> {
    reader: R,
    writer: W,
    id: u32,
}

impl<R: Read, W: Write> Client<R, W> {
    fn receive(&mut self) -> Result<Vec<u8>, String> {
        let mut header = [0u8; 4];
        self.reader
            .read_exact(&mut header)
            .map_err(|e| e.to_string())?;
        let length = u32::from_be_bytes(header) as usize;
        if !(1..=LIMIT + 1024).contains(&length) {
            return Err("SFTP response exceeds the packet limit".into());
        }
        let mut packet = vec![0; length];
        self.reader
            .read_exact(&mut packet)
            .map_err(|e| e.to_string())?;
        Ok(packet)
    }

    fn send(&mut self, packet: &[u8]) -> Result<(), String> {
        self.writer
            .write_all(&(packet.len() as u32).to_be_bytes())
            .and_then(|()| self.writer.write_all(packet))
            .and_then(|()| self.writer.flush())
            .map_err(|e| e.to_string())
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.send(&[1, 0, 0, 0, 3])?;
        let packet = self.receive()?;
        if packet.get(..5) != Some(&[2, 0, 0, 0, 3]) {
            return Err("the server did not negotiate SFTP v3".into());
        }
        Ok(())
    }

    fn request(&mut self, kind: u8, body: &[u8]) -> Result<Vec<u8>, String> {
        self.id += 1;
        let mut packet = vec![kind];
        packet.extend_from_slice(&self.id.to_be_bytes());
        packet.extend_from_slice(body);
        self.send(&packet)?;
        let reply = self.receive()?;
        if reply.get(1..5) != Some(self.id.to_be_bytes().as_slice()) {
            return Err("SFTP response request id mismatch".into());
        }
        Ok(reply)
    }

    fn status(packet: &[u8]) -> Result<u32, String> {
        if packet.first() != Some(&101) {
            return Err("expected an SFTP status response".into());
        }
        let code: [u8; 4] = packet
            .get(5..9)
            .ok_or("truncated SFTP status")?
            .try_into()
            .map_err(|_| "truncated SFTP status")?;
        Ok(u32::from_be_bytes(code))
    }

    fn ok(packet: &[u8]) -> Result<(), String> {
        match Self::status(packet)? {
            0 => Ok(()),
            code => Err(format!("SFTP operation failed (status {code})")),
        }
    }

    fn value(packet: &[u8], kind: u8) -> Result<&[u8], String> {
        if packet.first() == Some(&101) {
            return Err(format!(
                "SFTP operation failed (status {})",
                Self::status(packet)?
            ));
        }
        if packet.first() != Some(&kind) {
            return Err("unexpected SFTP response".into());
        }
        let length: [u8; 4] = packet
            .get(5..9)
            .ok_or("truncated SFTP string")?
            .try_into()
            .map_err(|_| "truncated SFTP string")?;
        packet
            .get(9..9 + u32::from_be_bytes(length) as usize)
            .ok_or_else(|| "truncated SFTP string".into())
    }

    fn transfer(&mut self, path: &str, contents: Option<&[u8]>) -> Result<Vec<u8>, String> {
        self.initialize()?;
        let mut open = Vec::new();
        string(&mut open, path.as_bytes());
        // READ or WRITE|CREAT|TRUNC; no remote shell, globbing, or local temporary file.
        open.extend_from_slice(&if contents.is_some() { 0x1au32 } else { 1u32 }.to_be_bytes());
        open.extend_from_slice(&0u32.to_be_bytes()); // empty attributes
        let packet = self.request(3, &open)?;
        let handle = Self::value(&packet, 102)?.to_vec();
        let mut result = Vec::new();
        if let Some(contents) = contents {
            for (index, chunk) in contents.chunks(CHUNK).enumerate() {
                let mut body = Vec::new();
                string(&mut body, &handle);
                body.extend_from_slice(&((index * CHUNK) as u64).to_be_bytes());
                string(&mut body, chunk);
                Self::ok(&self.request(6, &body)?)?;
            }
        } else {
            loop {
                let mut body = Vec::new();
                string(&mut body, &handle);
                body.extend_from_slice(&(result.len() as u64).to_be_bytes());
                body.extend_from_slice(&(CHUNK as u32).to_be_bytes());
                let packet = self.request(5, &body)?;
                if packet.first() == Some(&101) && Self::status(&packet)? == 1 {
                    break;
                }
                let chunk = Self::value(&packet, 103)?;
                if chunk.is_empty() || chunk.len() > CHUNK {
                    return Err("invalid SFTP data length".into());
                }
                if result.len() + chunk.len() > LIMIT {
                    return Err("remote file exceeds 1 MiB".into());
                }
                result.extend_from_slice(chunk);
            }
        }
        let mut close = Vec::new();
        string(&mut close, &handle);
        Self::ok(&self.request(4, &close)?)?;
        Ok(result)
    }
}

pub(super) fn transfer(
    target: &Target,
    settings: &Settings,
    path: &RemotePath,
    contents: Option<&[u8]>,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    if contents.is_some_and(|bytes| bytes.len() > LIMIT) {
        return Err("SSH URL writes are limited to 1 MiB".into());
    }
    connection::ensure_connection(target, settings, cancel)?;
    let mut command = connection::subsystem_command(target, settings, "sftp")?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::platform::process::spawn_group(&mut command)
        .map_err(|e| format!("starting SFTP: {e}"))?;
    let reader = child.stdout.take();
    let writer = child.stdin.take();
    let path = remote_path(path);
    let contents = contents.map(<[u8]>::to_vec);
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("dial-sftp".into())
        .spawn(move || {
            let result = match (reader, writer) {
                (Some(reader), Some(writer)) => Client {
                    reader,
                    writer,
                    id: 0,
                }
                .transfer(&path, contents.as_deref()),
                _ => Err("SFTP pipes unavailable".into()),
            };
            let _ = send.send(result);
        });
    let started = Instant::now();
    let result = match worker {
        Err(error) => Err(format!("starting SFTP relay: {error}")),
        Ok(_) => loop {
            if cancel.load(Ordering::Relaxed) {
                break Err("SFTP transfer cancelled; a write may be partial".into());
            }
            if started.elapsed() >= TIMEOUT {
                break Err("SFTP transfer timed out; a write may be partial".into());
            }
            match receive.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => break Err("SFTP relay ended without a result".into()),
            }
        },
    };
    // The leader is still owned; kill its group before reaping, including on protocol failure.
    let _ = crate::platform::process::kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
    crate::platform::process::forget_group(child.id());
    result
}

#[cfg(test)]
mod tests;

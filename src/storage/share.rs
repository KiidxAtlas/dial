//! End-to-end encrypted session sharing. Uploads contain only a redacted, gzip-compressed
//! transcript encrypted with a fresh AES-256-GCM key; the key is returned only in the URL fragment.
use crate::storage::session::Session;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use std::io::Read;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server_url: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: String::new(),
        }
    }
}

const MAGIC: &[u8; 8] = b"DIALSHR1";
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedShare {
    pub bytes: Vec<u8>,
    pub key_fragment: String,
}

/// Which records a share uploads. `ActiveBranch` is the default: abandoned branches and internal
/// records stay local. `WholeJournal` is an explicit diagnostic mode that uploads every record.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    ActiveBranch,
    WholeJournal,
}

/// The plaintext transcript (a JSON array of records) for the session file at `path` under `scope`.
/// `dial share` and `/share` both build their payload here, then seal it with `encrypt_transcript`.
pub fn transcript(path: &Path, scope: Scope) -> Result<Vec<u8>> {
    let records = match scope {
        Scope::ActiveBranch => Session::read(path)?.share_records(),
        Scope::WholeJournal => whole_journal_records(path)?,
    };
    Ok(serde_json::to_vec(&records)?)
}

fn whole_journal_records(path: &Path) -> Result<Vec<Value>> {
    let source = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    source
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<Value>(line).context("session journal line is not JSON")
        })
        .collect()
}

pub fn encrypt_transcript(transcript: &[u8]) -> Result<EncryptedShare> {
    let mut value: Value =
        serde_json::from_slice(transcript).context("share input must be a session JSON value")?;
    crate::storage::redact::redact_value(&mut value);
    let redacted = serde_json::to_vec(&value)?;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&redacted)?;
    let mut compressed = encoder.finish()?;
    let rng = SystemRandom::new();
    let mut key = [0u8; 32];
    let mut nonce = [0u8; 12];
    rng.fill(&mut key)
        .map_err(|_| anyhow::anyhow!("secure random generator failed"))?;
    rng.fill(&mut nonce)
        .map_err(|_| anyhow::anyhow!("secure random generator failed"))?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| anyhow::anyhow!("AES-256-GCM unavailable"))?;
    let sealing = aead::LessSafeKey::new(unbound);
    sealing
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(MAGIC),
            &mut compressed,
        )
        .map_err(|_| anyhow::anyhow!("share encryption failed"))?;
    let mut bytes = Vec::with_capacity(MAGIC.len() + nonce.len() + compressed.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&compressed);
    Ok(EncryptedShare {
        bytes,
        key_fragment: URL_SAFE_NO_PAD.encode(key),
    })
}

#[cfg(test)]
pub fn decrypt_share(bytes: &[u8], key_fragment: &str) -> Result<Vec<u8>> {
    ensure!(
        bytes.len() >= MAGIC.len() + 12 + 16 && bytes.starts_with(MAGIC),
        "invalid encrypted share payload"
    );
    let key = URL_SAFE_NO_PAD
        .decode(key_fragment)
        .context("invalid share key fragment")?;
    ensure!(key.len() == 32, "share key must be 256 bits");
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&bytes[MAGIC.len()..MAGIC.len() + 12]);
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| anyhow::anyhow!("AES-256-GCM unavailable"))?;
    let opening = aead::LessSafeKey::new(unbound);
    let mut plain = bytes[MAGIC.len() + 12..].to_vec();
    let plain = opening
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(MAGIC),
            &mut plain,
        )
        .map_err(|_| anyhow::anyhow!("share authentication failed"))?;
    let mut decoder = flate2::read::GzDecoder::new(&*plain);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .context("decompressing shared transcript")?;
    Ok(output)
}

/// Upload encrypted bytes to an operator-configured endpoint. Response is a URL or JSON `{url}`.
pub fn upload(encrypted: &EncryptedShare, endpoint: &str) -> Result<String> {
    ensure!(
        is_safe_share_url(endpoint),
        "share endpoint must use HTTPS (HTTP is allowed only on loopback)"
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();
    let response = agent
        .put(endpoint)
        .header("content-type", "application/octet-stream")
        .send(&encrypted.bytes)
        .context("uploading encrypted session")?;
    let body = response
        .into_body()
        .read_to_vec()
        .context("reading share upload response")?;
    let text = String::from_utf8(body).context("share server returned non-UTF-8 response")?;
    let url = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or(text.trim().to_owned());
    ensure!(
        is_safe_share_url(&url),
        "share server did not return a safe URL"
    );
    Ok(format!(
        "{}#{}",
        url.trim_end_matches('#'),
        encrypted.key_fragment
    ))
}

/// HTTPS anywhere, or HTTP only to the exact loopback hosts.
fn is_safe_share_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => match parsed.scheme() {
            "https" => true,
            "http" => matches!(parsed.host_str(), Some("127.0.0.1" | "localhost")),
            _ => false,
        },
        Err(_) => false,
    }
}

/// Create a secret GitHub gist with ciphertext only. `gh` receives base64 data on stdin.
pub fn upload_gist(encrypted: &EncryptedShare) -> Result<String> {
    use std::{
        io::Write,
        process::{Command, Stdio},
        sync::mpsc::RecvTimeoutError,
        time::Duration,
    };
    const GH_GIST_TIMEOUT: Duration = Duration::from_secs(120);
    let mut command = Command::new("gh");
    command
        .args([
            "gist",
            "create",
            "--secret",
            "--filename",
            "session.dialshare",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::platform::process::spawn_group(&mut command)
        .context("creating secret gist (install and authenticate the GitHub CLI `gh`)")?;
    let pid = child.id();
    let stdin = child.stdin.take().context("opening gh stdin")?;
    let payload = URL_SAFE_NO_PAD.encode(&encrypted.bytes);
    // Write on a thread: wait_with_output drains stdout/stderr concurrently, so a chatty gh cannot
    // fill its stderr pipe while we are still blocked writing the payload.
    let _writer = std::thread::spawn(move || {
        let mut stdin = stdin;
        let _ = stdin.write_all(payload.as_bytes());
    });
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    // The deadline kills the whole gh group, which also closes the pipes so the writer unblocks.
    let output = match rx.recv_timeout(GH_GIST_TIMEOUT) {
        Ok(output) => output.context("waiting for gh gist create")?,
        Err(RecvTimeoutError::Timeout) => {
            let _ = crate::platform::process::kill_group(pid);
            anyhow::bail!(
                "gh gist create timed out after {}s",
                GH_GIST_TIMEOUT.as_secs()
            );
        }
        Err(RecvTimeoutError::Disconnected) => {
            anyhow::bail!("gh gist create worker stopped");
        }
    };
    ensure!(
        output.status.success(),
        "gh gist create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let url = String::from_utf8(output.stdout)
        .context("gh returned a non-UTF-8 gist URL")?
        .trim()
        .to_owned();
    ensure!(
        url.starts_with("https://gist.github.com/"),
        "gh returned an unexpected gist URL"
    );
    Ok(format!("{url}#{}", encrypted.key_fragment))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encryption_roundtrip_redacts_secrets() {
        let source = br#"{"messages":[{"text":"token sk-ant-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH1234567890"}]}"#;
        let encrypted = encrypt_transcript(source).unwrap();
        assert!(!encrypted.bytes.windows(7).any(|w| w == b"sk-ant-"));
        let decoded: Value = serde_json::from_slice(
            &decrypt_share(&encrypted.bytes, &encrypted.key_fragment).unwrap(),
        )
        .unwrap();
        assert!(!decoded.to_string().contains("sk-ant-"));
        assert!(decrypt_share(&encrypted.bytes, &URL_SAFE_NO_PAD.encode([1u8; 32])).is_err());
    }
}

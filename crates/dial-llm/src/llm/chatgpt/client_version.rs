//! The Codex client version Dial reports to the ChatGPT backend (`version` header and the model
//! list's `client_version`). The backend gates newer models on it, so a stale value hides models
//! or refuses them with `model_not_found`. The reported version is the newest of the release
//! Dial was built against, the published `@openai/codex` release (checked at most once a day
//! during ChatGPT model discovery and cached) and a recent local Codex model cache.
use crate::config::settings_store::write_private;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The Codex release this build was verified against; never report anything older.
pub(crate) const BUNDLED: &str = "0.159.0";
pub(super) const REGISTRY_URL: &str = "https://registry.npmjs.org/@openai/codex/latest";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const MAX_REGISTRY_JSON: u64 = 256 * 1024;
const CACHE_NAME: &str = "codex-client-version.json";

static CURRENT: RwLock<Option<String>> = RwLock::new(None);

#[derive(Default, Serialize, Deserialize)]
struct Stamp {
    checked_at: u64,
    version: Option<String>,
}

/// The version to report now. The first call in a process resolves it from local caches only.
pub fn current() -> String {
    if let Some(version) = CURRENT.read().clone() {
        return version;
    }
    let cached = crate::config::paths::user_dir()
        .ok()
        .and_then(|dir| read_stamp(&cache_path(&dir)).version);
    let codex = super::codex_model_metadata();
    let resolved = newest([cached.as_deref(), codex_version(codex.as_ref())]);
    *CURRENT.write() = Some(resolved.clone());
    resolved
}

/// Refreshes the published release when the daily check is due, then updates [`current`].
/// Network failures keep the previous cached release; they never fail discovery.
pub(super) fn refresh(dir: &Path, codex: Option<&Value>, registry: &str) -> String {
    let path = cache_path(dir);
    let mut stamp = read_stamp(&path);
    let now = now();
    if due(now, stamp.checked_at) {
        stamp = Stamp {
            checked_at: now,
            version: fetch_latest(registry).or(stamp.version),
        };
        if let Ok(bytes) = serde_json::to_vec(&stamp) {
            let _ = write_private(&path, &bytes);
        }
    }
    let resolved = newest([stamp.version.as_deref(), codex_version(codex)]);
    *CURRENT.write() = Some(resolved.clone());
    resolved
}

fn cache_path(dir: &Path) -> std::path::PathBuf {
    crate::config::paths::cache_file(dir, CACHE_NAME)
}

fn read_stamp(path: &Path) -> Stamp {
    crate::platform::atomic::read_private(path)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Stamp>(&bytes).ok())
        .map(|stamp| Stamp {
            version: stamp.version.filter(|v| parse(v).is_some()),
            ..stamp
        })
        .unwrap_or_default()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A future stamp (clock correction or corrupt state) must not disable checks indefinitely.
fn due(now: u64, last: u64) -> bool {
    now.checked_sub(last)
        .is_none_or(|elapsed| elapsed >= CHECK_INTERVAL_SECS)
}

fn fetch_latest(url: &str) -> Option<String> {
    let mut response = crate::llm::agent_with(5, true, None, None)
        .get(url)
        .header("Accept", "application/json")
        .call()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_REGISTRY_JSON + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_REGISTRY_JSON {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value["version"]
        .as_str()
        .filter(|v| parse(v).is_some())
        .map(str::to_owned)
}

fn codex_version(codex: Option<&Value>) -> Option<&str> {
    codex?["client_version"].as_str()
}

/// A stable `MAJOR.MINOR.PATCH` release; pre-releases and anything else are ignored.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let mut next = || -> Option<u64> {
        let part = parts.next()?;
        (!part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    let parsed = (next()?, next()?, next()?);
    parts.next().is_none().then_some(parsed)
}

/// The newest valid candidate, never older than [`BUNDLED`].
fn newest<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> String {
    candidates
        .into_iter()
        .flatten()
        .filter_map(|v| parse(v).map(|key| (key, v)))
        .chain(parse(BUNDLED).map(|key| (key, BUNDLED)))
        .max_by_key(|(key, _)| *key)
        .map_or_else(|| BUNDLED.to_owned(), |(_, v)| v.to_owned())
}

#[cfg(test)]
mod tests;

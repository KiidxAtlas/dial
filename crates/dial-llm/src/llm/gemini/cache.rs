//! Bounded, process-local explicit caches. No credentials or prompt text survive a request.
//! Unknown create outcomes and process shutdown are bounded by the server-side TTL, not retries.
use super::super::{self as llm, Endpoint, Interrupted, profiles::GeminiCache};
use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_ENTRIES: usize = 32;
static SLOTS: LazyLock<Mutex<Vec<Slot>>> = LazyLock::new(|| Mutex::new(Vec::new()));

struct Slot {
    scope: String,
    key: String,
    expires: Instant,
    usable_until: Option<Instant>,
    name: Option<String>,
    busy: bool,
    small: bool,
}

/// Exact, conservative allow-list; never classify Gemini 4+ or custom names by substring.
fn supported(model: &str) -> bool {
    matches!(
        model.strip_prefix("models/").unwrap_or(model),
        "gemini-2.5-pro"
            | "gemini-2.5-flash"
            | "gemini-2.5-flash-lite"
            | "gemini-3-pro-preview"
            | "gemini-3-flash-preview"
            | "gemini-3.1-pro-preview"
    )
}
fn base<'a>(endpoint: &Endpoint<'a>) -> Option<&'a str> {
    let (base, method) = endpoint.url.rsplit_once("/models/")?;
    let model = endpoint
        .model
        .strip_prefix("models/")
        .unwrap_or(endpoint.model);
    (method == format!("{model}:streamGenerateContent?alt=sse")).then_some(base)
}

struct Prefix {
    body: Value,
    contents: usize,
}
fn prefix(body: &Value) -> Option<Prefix> {
    let mut cached = json!({});
    for field in ["systemInstruction", "tools", "toolConfig"] {
        if let Some(value) = body.get(field) {
            cached[field] = value.clone();
        }
    }
    // Only the first immutable user content, and only once it has a successor. Never store
    // the current tail, model thoughts, tool responses, or inline/file media as a fixed prefix.
    let contents = body["contents"].as_array()?;
    let count = usize::from(
        contents.len() > 1
            && contents[0]["role"] == "user"
            && contents[0]["parts"].as_array().is_some_and(|parts| {
                !parts.is_empty()
                    && parts.iter().all(|part| {
                        part.as_object().is_some_and(|part| {
                            part.len() == 1 && part.get("text").is_some_and(Value::is_string)
                        })
                    })
            }),
    );
    if count > 0 {
        cached["contents"] = json!([contents[0]]);
    }
    (!cached.as_object()?.is_empty()).then_some(Prefix {
        body: cached,
        contents: count,
    })
}

fn apply(body: &mut Value, prefix: &Prefix, name: &str) {
    for field in ["systemInstruction", "tools", "toolConfig"] {
        body.as_object_mut()
            .expect("Gemini body object")
            .remove(field);
    }
    if prefix.contents > 0 {
        body["contents"]
            .as_array_mut()
            .expect("Gemini contents array")
            .drain(..prefix.contents);
    }
    body["cachedContent"] = json!(name);
}

struct Reservation(String);
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(slot) = SLOTS.lock().iter_mut().find(|slot| slot.key == self.0) {
            slot.busy = false;
        }
    }
}

#[derive(Debug)]
pub(super) struct Used {
    key: String,
}

/// Return a handle for failure invalidation. A first observation only registers the digest;
/// paid storage is created only after a repeated prefix passes Google's actual token count.
pub(super) fn prepare(
    endpoint: &Endpoint<'_>,
    body: &mut Value,
    config: Option<&GeminiCache>,
    scope: &str,
    interrupt: &Arc<AtomicBool>,
    timeout: u64,
) -> Result<Option<Used>> {
    let Some(config) = config.filter(|_| supported(endpoint.model)) else {
        return Ok(None);
    };
    let Some(base) = base(endpoint) else {
        return Ok(None);
    };
    let Some(prefix) = prefix(body) else {
        return Ok(None);
    };
    let bytes = serde_json::to_vec(&prefix.body)?;
    if bytes.len() > config.max_prefix_bytes {
        return Ok(None);
    }
    let key = format!(
        "{:x}",
        Sha256::digest(
            [
                scope.as_bytes(),
                &bytes,
                &config.ttl_secs.to_le_bytes(),
                &config.min_tokens.to_le_bytes()
            ]
            .concat()
        )
    );
    let now = Instant::now();
    {
        let mut slots = SLOTS.lock();
        // Expired remote objects already have a TTL; no detached cleanup tasks or credentials.
        slots.retain(|slot| slot.busy || slot.expires > now);
        if let Some(slot) = slots.iter_mut().find(|slot| slot.key == key) {
            if let Some(name) = &slot.name {
                if slot
                    .usable_until
                    .is_none_or(|until| until <= now + Duration::from_secs(5))
                {
                    return Ok(None);
                }
                apply(body, &prefix, name);
                return Ok(Some(Used { key }));
            }
            if slot.busy || slot.small {
                return Ok(None);
            }
            slot.busy = true;
        } else {
            if slots.len() >= MAX_ENTRIES
                || slots.iter().filter(|slot| slot.scope == scope).count() >= config.max_entries
            {
                return Ok(None);
            }
            slots.push(Slot {
                scope: scope.into(),
                key,
                expires: now + Duration::from_secs(config.ttl_secs),
                usable_until: None,
                name: None,
                busy: false,
                small: false,
            });
            return Ok(None);
        }
    }
    let _reservation = Reservation(key.clone());
    let model = format!(
        "models/{}",
        endpoint
            .model
            .strip_prefix("models/")
            .unwrap_or(endpoint.model)
    );
    let mut count_request = prefix.body.clone();
    count_request["model"] = json!(model);
    let count = post(
        endpoint,
        &format!("{base}/{model}:countTokens"),
        &json!({"generateContentRequest":count_request}),
        interrupt,
        timeout,
    )?;
    let tokens = count["totalTokens"]
        .as_u64()
        .context("Gemini countTokens omitted totalTokens")?;
    // A conservative floor works across the supported models; Google remains the authority.
    if tokens < u64::from(config.min_tokens.max(4096)) {
        if let Some(slot) = SLOTS.lock().iter_mut().find(|slot| slot.key == key) {
            slot.small = true;
        }
        return Ok(None);
    }
    let mut create = prefix.body.clone();
    let created_at = Instant::now();
    // An ambiguous POST outcome must not create another billed object on the next retry.
    if let Some(slot) = SLOTS.lock().iter_mut().find(|slot| slot.key == key) {
        slot.small = true;
        // Retain ambiguous outcomes for the requested TTL plus the bounded create deadline.
        slot.expires = created_at + Duration::from_secs(config.ttl_secs + 30);
    }
    create["model"] = json!(model);
    create["ttl"] = json!(format!("{}s", config.ttl_secs));
    let created = post(
        endpoint,
        &format!("{base}/cachedContents"),
        &create,
        interrupt,
        timeout,
    )?;
    let name = created["name"]
        .as_str()
        .filter(|name| valid_name(name))
        .context("Gemini cache response omitted a valid resource name")?
        .to_owned();
    // Commit the known name before any subsequent fallible operation so cleanup can find it.
    if let Some(slot) = SLOTS.lock().iter_mut().find(|slot| slot.key == key) {
        slot.name = Some(name.clone());
    }
    let used = Used { key };
    let lifetime = created["expireTime"]
        .as_str()
        .and_then(expiration)
        .and_then(|time| time.duration_since(SystemTime::now()).ok());
    let Some(lifetime) = lifetime.filter(|life| *life > Duration::from_secs(5)) else {
        invalidate(endpoint, &used);
        anyhow::bail!("Gemini returned an expired or invalid cache expiration");
    };
    if let Some(slot) = SLOTS.lock().iter_mut().find(|slot| slot.key == used.key) {
        slot.usable_until = Some(
            (created_at + Duration::from_secs(config.ttl_secs)).min(Instant::now() + lifetime),
        );
    }
    if interrupt.load(Ordering::SeqCst) {
        invalidate(endpoint, &used);
        return Err(Interrupted.into());
    }
    apply(body, &prefix, &name);
    Ok(Some(used))
}

fn valid_name(name: &str) -> bool {
    name.strip_prefix("cachedContents/").is_some_and(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    })
}
fn expiration(value: &str) -> Option<SystemTime> {
    let whole = value.strip_suffix('Z')?.split('.').next()?;
    let seconds = crate::platform::time::parse_utc(whole)?;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).ok()?))
}

fn headers<'a>(endpoint: &'a Endpoint<'a>) -> Vec<(&'a str, &'a str)> {
    let mut headers = endpoint.headers.to_vec();
    if let Some(key) = endpoint.auth {
        headers.push(("x-goog-api-key", key));
    }
    headers
}
fn post(
    endpoint: &Endpoint<'_>,
    url: &str,
    body: &Value,
    interrupt: &Arc<AtomicBool>,
    timeout: u64,
) -> Result<Value> {
    if interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    let result = (|| {
        let mut response = llm::send_with_headers(
            url,
            None,
            body,
            timeout.clamp(1, 30),
            None,
            Some(Arc::clone(interrupt)),
            &headers(endpoint),
        )?;
        let status = response.status().as_u16();
        let delay = llm::retry::server_delay(response.headers());
        let bytes = llm::read_bounded(response.body_mut().as_reader(), llm::ERROR_EXCERPT_BYTES)?;
        if !(200..300).contains(&status) {
            return Err(llm::http_error(status, &bytes, endpoint.auth, delay));
        }
        ensure!(
            !bytes.truncated,
            "Gemini cache response exceeded size limit"
        );
        serde_json::from_slice(&bytes.data).context("invalid Gemini cache response")
    })();
    if result.is_err() && interrupt.load(Ordering::SeqCst) {
        return Err(Interrupted.into());
    }
    result.map_err(|error| match error.downcast::<llm::TransportError>() {
        Ok(mut error) => {
            error.message = llm::redact(&error.message, endpoint.auth);
            error.into()
        }
        Err(error) if error.is::<llm::HttpError>() => error,
        Err(error) => llm::response_error(error, endpoint.auth),
    })
}

/// No replay on cachedContent rejection: remove the resource and surface the original error.
/// Best-effort deletion has an independent two-second cancellation/transport budget; if it fails,
/// the server TTL still bounds storage. This never spawns background work or retains credentials.
pub(super) fn invalidate(endpoint: &Endpoint<'_>, used: &Used) {
    let name = {
        let mut slots = SLOTS.lock();
        slots
            .iter_mut()
            .find(|slot| slot.key == used.key)
            .and_then(|slot| {
                // Keep a tombstone until TTL even if DELETE fails; retries cannot accumulate
                // unbounded billed resources by repeatedly releasing and recreating this slot.
                slot.small = true;
                slot.name.take()
            })
    };
    if let (Some(base), Some(name)) = (base(endpoint), name) {
        let url = format!("{base}/{name}");
        let agent = llm::agent_for(&url, 2, Some(Arc::new(AtomicBool::new(false))), None);
        let mut request = agent.delete(&url);
        for (key, value) in headers(endpoint) {
            request = request.header(key, value);
        }
        let _ = request.call();
    }
}

#[cfg(test)]
mod tests;

//! Shared bounds and authentication for native image services. Submission is never retried.
use super::{MAX_RESPONSE_BYTES, Target, Wire};
use crate::config::types::Config;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Bridge the borrowed tool interrupt into the HTTP transport's owned cancellation flag.
pub(super) fn with_cancel<T>(cancel: &AtomicBool, action: impl FnOnce(Arc<AtomicBool>) -> T) -> T {
    let shared = Arc::new(AtomicBool::new(cancel.load(Ordering::SeqCst)));
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let shared_ref = &shared;
        let done_ref = &done;
        let watcher = scope.spawn(move || {
            while !done_ref.load(Ordering::SeqCst) {
                if cancel.load(Ordering::SeqCst) {
                    shared_ref.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(25));
            }
        });
        struct Stop<'a>(&'a AtomicBool, std::thread::Thread);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
                self.1.unpark();
            }
        }
        let _stop = Stop(&done, watcher.thread().clone());
        action(shared.clone())
    })
}

pub(super) struct Client<'a> {
    target: &'a Target,
    config: &'a Config,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

impl<'a> Client<'a> {
    pub(super) fn new(target: &'a Target, config: &'a Config, cancel: Arc<AtomicBool>) -> Self {
        Self {
            target,
            config,
            cancel,
            started: Instant::now(),
        }
    }

    pub(super) fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.target.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    pub(super) fn check(&self) -> Result<()> {
        ensure!(
            !self.cancel.load(Ordering::SeqCst),
            "image generation interrupted; a submitted remote job may still be running"
        );
        ensure!(
            self.config.image.timeout_secs == 0
                || self.started.elapsed() < Duration::from_secs(self.config.image.timeout_secs),
            "image generation timed out; a submitted remote job may still be running"
        );
        Ok(())
    }

    pub(super) fn pause(&self) -> Result<()> {
        for _ in 0..10 {
            self.check()?;
            std::thread::sleep(Duration::from_millis(50));
        }
        self.check()
    }

    pub(super) fn agent(&self) -> Result<ureq::Agent> {
        self.check()?;
        let seconds = self.config.image.timeout_secs;
        let remaining = if seconds == 0 {
            0
        } else {
            (Duration::from_secs(seconds)
                .saturating_sub(self.started.elapsed())
                .as_secs_f64()
                .ceil() as u64)
                .max(1)
        };
        Ok(crate::llm::agent_with(
            remaining,
            !crate::llm::is_local_url(&self.target.base_url),
            Some(self.cancel.clone()),
            None,
        ))
    }

    pub(super) fn post(&self, url: &str, body: &Value) -> Result<Value> {
        self.json("POST", url, Some(body)).context(
            "image submission was not retried; if the service received it, a remote job may have been accepted",
        )
    }

    pub(super) fn get(&self, url: &str) -> Result<Value> {
        self.json("GET", url, None)
    }

    fn json(&self, method: &str, url: &str, body: Option<&Value>) -> Result<Value> {
        validate_service_url(&self.target.base_url, url, self.target.wire)?;
        let headers = crate::llm::model_config::resolve_headers(&self.target.headers)?;
        // Regional BFL polling may receive the BFL key, never unrelated custom headers.
        let same_origin = parsed(url)?.origin() == parsed(&self.target.base_url)?.origin();
        let agent = self.agent()?;
        // Build the two concrete request types separately: ureq tracks request-body capability.
        let auth = if self.target.key.is_empty() {
            None
        } else {
            Some(match self.target.wire {
                Wire::Gemini => ("x-goog-api-key", self.target.key.clone()),
                Wire::BlackForestLabs => ("x-key", self.target.key.clone()),
                Wire::Fal => ("authorization", format!("Key {}", self.target.key)),
                _ => ("authorization", format!("Bearer {}", self.target.key)),
            })
        };
        let sent = if method == "POST" {
            let mut builder = agent.post(url);
            if let Some((name, value)) = &auth {
                builder = builder.header(*name, value);
            }
            for (name, value) in headers.pairs().filter(|_| same_origin) {
                builder = builder.header(name, value);
            }
            builder.send_json(body.context("missing image request body")?)
        } else {
            let mut builder = agent.get(url);
            if let Some((name, value)) = &auth {
                builder = builder.header(*name, value);
            }
            for (name, value) in headers.pairs().filter(|_| same_origin) {
                builder = builder.header(name, value);
            }
            builder.call()
        };
        let mut response = sent.map_err(|error| self.redact(headers.redact(anyhow!(error))))?;
        let status = response.status().as_u16();
        let raw = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|error| self.redact(headers.redact(anyhow!(error))))?;
        self.check()?;
        ensure!(
            (200..300).contains(&status),
            "image service failed (HTTP {status}); submission is not retried"
        );
        serde_json::from_slice(&raw).context("image service returned invalid JSON")
    }

    pub(super) fn redact(&self, error: anyhow::Error) -> anyhow::Error {
        let message = format!("{error:#}");
        let message = if self.target.key.is_empty() {
            message
        } else {
            message.replace(&self.target.key, "[redacted]")
        };
        anyhow!(crate::storage::redact::redact_text(&message).into_owned())
    }

    /// Provider-supplied diagnostic text. Withheld when custom headers are attached, because a
    /// peer can echo header values (possibly escaped or truncated), as `ResolvedHeaders::redact`
    /// does for transport errors.
    pub(super) fn provider_text(&self, text: String) -> String {
        if self.target.headers.is_empty() {
            text
        } else {
            "details withheld to protect custom headers".to_owned()
        }
    }

    /// `error` built from a provider response, or `summary` when custom headers are attached.
    pub(super) fn withhold(&self, error: anyhow::Error, summary: &str) -> anyhow::Error {
        if self.target.headers.is_empty() {
            error
        } else {
            anyhow!("{summary}; details withheld to protect custom headers")
        }
    }

    /// Download output without service keys or custom headers, and never follow redirects.
    pub(super) fn download(&self, url: &str) -> Result<Vec<u8>> {
        validate_download_url(&self.target.base_url, url, self.target.wire)?;
        let agent = self.agent()?;
        let mut response = agent
            .get(url)
            .call()
            .map_err(|_| anyhow!("image download transport failed"))?;
        ensure!(
            response.status().is_success(),
            "image download failed (HTTP {})",
            response.status().as_u16()
        );
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .context("reading generated image download")?;
        self.check()?;
        Ok(bytes)
    }
}

/// Validate configured endpoints without imposing chat-completions path normalization.
pub(super) fn validate_base(base: &str) -> Result<()> {
    let url = parsed(base)?;
    ensure!(
        url.query().is_none(),
        "image service base URL must not contain a query string"
    );
    Ok(())
}

fn parsed(url: &str) -> Result<url::Url> {
    let url = url::Url::parse(url).context("invalid image service URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "image service URLs must not contain user information or fragments"
    );
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http" && crate::llm::is_local_url(url.as_str())),
        "image service requires HTTPS (HTTP is allowed only for local services)"
    );
    Ok(url)
}

fn validate_service_url(base: &str, next: &str, wire: Wire) -> Result<()> {
    let base = parsed(base)?;
    let next = parsed(next)?;
    if base.origin() == next.origin() {
        return Ok(());
    }
    // BFL's documented global routing sends a regional polling URL, not necessarily the API origin.
    let bfl = |url: &url::Url| {
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.host_str().is_some_and(|host| host.ends_with(".bfl.ai"))
    };
    ensure!(
        wire == Wire::BlackForestLabs && bfl(&base) && bfl(&next),
        "image service returned a polling URL outside its authenticated origin"
    );
    Ok(())
}

fn validate_download_url(base: &str, next: &str, wire: Wire) -> Result<()> {
    let base = parsed(base)?;
    let next = parsed(next)?;
    if base.origin() == next.origin() {
        return Ok(());
    }
    ensure!(
        next.scheme() == "https" && next.port_or_known_default() == Some(443),
        "remote image downloads require HTTPS on port 443"
    );
    let host = next.host_str().context("image download has no host")?;
    let allowed = match wire {
        Wire::BlackForestLabs => host.starts_with("delivery.") && host.ends_with(".bfl.ai"),
        Wire::Fal => host == "fal.media" || host.ends_with(".fal.media"),
        Wire::Replicate => host == "replicate.delivery" || host.ends_with(".replicate.delivery"),
        _ => false,
    };
    if !allowed {
        bail!("image service returned an output URL outside its documented delivery hosts");
    }
    Ok(())
}

#[cfg(test)]
mod tests;

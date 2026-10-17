//! Provider OAuth sign-in.
//!
//! Each provider module exposes a [`Flow`]: an interactive login that returns [`Credentials`], an
//! optional refresh, and a request-time [`Access`] shaper. Grants are stored per provider in the
//! private user directory (`oauth-<id>.json`, owner-only, cross-process locked) and refreshed
//! under the lock immediately before a request; nothing is written inside the workspace.
//!
//! Interaction is abstracted by [`Interaction`]: the TUI shows the verification URL/code as a
//! banner and opens the browser, while `dial login <provider>` can also prompt on the terminal.
use super::auth::SecretToken;
use super::model_config::ApiProtocol;
use crate::storage::settings_store::{
    locked, locked_cancellable, locked_for, read_private, write_private,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
};

pub mod util;

mod alibaba_coding_plan;
mod alibaba_token_plan;
mod cloudflare_ai_gateway;
pub mod copilot;
mod cursor;
mod devin;
mod factory;
mod gitlab;
mod google;
mod kilo;
mod kimi;
pub mod muse_code;
mod perplexity;
mod snowflake;
mod xai;
pub mod xiaomi;
mod zai;

/// Refresh grants this long before their recorded expiry.
const REFRESH_MARGIN_MS: u64 = 60_000;

/// A stored OAuth grant. Its fields are the standard OAuth credential fields; `extra` keeps
/// provider-specific state (for example a minted API key).
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Credentials {
    pub access: String,
    pub refresh: String,
    /// Unix milliseconds, already reduced by the provider's skew; [`util::NEVER_EXPIRES`] never expires.
    pub expires_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enterprise_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("project_id", &self.project_id)
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    pub fn expired(&self, margin_ms: u64) -> bool {
        self.expires_at_ms != util::NEVER_EXPIRES
            && self.expires_at_ms <= util::now_ms().saturating_add(margin_ms)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.access.is_empty() && self.access.bytes().all(|b| b.is_ascii_graphic()),
            "sign-in returned an empty or invalid access token"
        );
        ensure!(
            self.refresh.bytes().all(|b| b.is_ascii_graphic()),
            "sign-in returned an invalid refresh token"
        );
        Ok(())
    }
    /// String field from `extra`.
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(|v| v.as_str())
    }
}

/// Request-time credential shape for one provider call.
pub struct Access {
    /// Sent as `Authorization: Bearer` unless the transport documents otherwise.
    pub token: SecretToken,
    /// Overrides the selected base URL (Copilot plan endpoint, GitLab gateway, …).
    pub base_url: Option<String>,
    /// Extra request headers required by the provider (non-secret identity/routing values,
    /// or provider-issued gateway headers).
    pub headers: Vec<(String, String)>,
    /// Google Cloud project for Cloud Code Assist.
    pub project_id: Option<String>,
    pub account_id: Option<String>,
}

impl fmt::Debug for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Access")
            .field("base_url", &self.base_url)
            .field("project_id", &self.project_id)
            .finish_non_exhaustive()
    }
}

/// The default request shape: the access token as a bearer credential.
pub fn bearer(credentials: &Credentials) -> Result<Access> {
    Ok(Access {
        token: SecretToken::new(credentials.access.clone()),
        base_url: None,
        headers: Vec::new(),
        project_id: credentials.project_id.clone(),
        account_id: credentials.account_id.clone(),
    })
}

/// How a login flow talks to the user.
pub trait Interaction: Send + Sync {
    /// Show a URL to visit (the flow has already tried to open it) and optional instructions
    /// such as a device code.
    fn auth(&self, url: &str, instructions: Option<&str>);
    /// Progress text.
    fn progress(&self, message: &str);
    /// Ask for one line. Implementations without input return `default` when `allow_empty`,
    /// and an error explaining how to sign in from a terminal otherwise.
    fn prompt(&self, message: &str, placeholder: Option<&str>, allow_empty: bool)
    -> Result<String>;
}

/// Inputs to a login flow.
pub struct Login<'a> {
    pub ui: &'a dyn Interaction,
    pub cancel: Arc<AtomicBool>,
}

impl Login<'_> {
    /// Try the browser, then always show the URL and instructions.
    pub fn open(&self, url: &str, instructions: Option<&str>) {
        util::open_browser(url);
        self.ui.auth(url, instructions);
    }
}

type LoginFn = fn(&Login<'_>) -> Result<Credentials>;
type RefreshFn = fn(&Credentials, &Arc<AtomicBool>) -> Result<Credentials>;
type AccessFn = fn(&Credentials) -> Result<Access>;

/// One provider's OAuth implementation.
pub struct Flow {
    /// Dial provider id.
    pub id: &'static str,
    pub name: &'static str,
    /// Inference base URL used when the selection has none.
    pub base_url: &'static str,
    /// Wire protocol for inference with this grant.
    pub protocol: ApiProtocol,
    /// Login requires typed input with no safe default; the TUI points to `dial login <id>`.
    pub needs_terminal: bool,
    /// Usage is covered by a plan/subscription rather than metered per token (no cost display).
    pub subscription: bool,
    pub login: LoginFn,
    /// `None`: the grant does not expire or cannot be refreshed (sign in again).
    pub refresh: Option<RefreshFn>,
    pub access: AccessFn,
}

static FLOWS: &[&Flow] = &[
    &copilot::FLOW,
    &google::GEMINI_CLI,
    &google::ANTIGRAVITY,
    &kimi::FLOW,
    &zai::FLOW,
    &xai::FLOW,
    &gitlab::FLOW,
    &perplexity::FLOW,
    &kilo::FLOW,
    &factory::FLOW,
    &devin::FLOW,
    &cursor::FLOW,
    &snowflake::FLOW,
    &cloudflare_ai_gateway::FLOW,
    &xiaomi::FLOW,
    &muse_code::FLOW,
    &alibaba_coding_plan::FLOW,
    &alibaba_token_plan::FLOW,
];

/// Every OAuth provider Dial can sign in to.
pub fn flows() -> impl Iterator<Item = &'static Flow> {
    FLOWS.iter().copied()
}

pub fn flow(id: &str) -> Option<&'static Flow> {
    flows().find(|flow| flow.id == id)
}

fn require(id: &str) -> Result<&'static Flow> {
    flow(id).with_context(|| {
        format!(
            "unknown OAuth provider {id:?}; known providers: {}",
            flows().map(|f| f.id).collect::<Vec<_>>().join(", ")
        )
    })
}

fn path(user_dir: &Path, id: &str) -> PathBuf {
    user_dir.join(format!("oauth-{id}.json"))
}

fn load(path: &Path) -> Result<Option<Credentials>> {
    let Some(bytes) = read_private(path)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .context("stored OAuth credentials are invalid; sign in again")
}

fn save(path: &Path, credentials: &Credentials) -> Result<()> {
    write_private(path, &serde_json::to_vec(credentials)?)
}

pub fn signed_in(user_dir: &Path, id: &str) -> bool {
    flow(id).is_some() && load(&path(user_dir, id)).ok().flatten().is_some()
}

/// Public identity of a stored grant (email or account id), for display.
pub fn identity(user_dir: &Path, id: &str) -> Option<String> {
    let credentials = load(&path(user_dir, id)).ok().flatten()?;
    credentials
        .email
        .or(credentials.org_name)
        .or(credentials.account_id)
}

/// Enterprise host recorded at login (Copilot GitHub Enterprise), when the grant has one.
pub fn enterprise_url(user_dir: &Path, id: &str) -> Option<String> {
    load(&path(user_dir, id)).ok().flatten()?.enterprise_url
}

/// The endpoint a signed-in grant routes to by itself (a Cloudflare gateway or Snowflake account
/// built from its identifiers, a regional plan URL). Catalog rows and request routing both read it,
/// so they name the same route.
pub fn grant_route(user_dir: &Path, id: &str) -> Option<String> {
    let flow = flow(id)?;
    let credentials = load(&path(user_dir, id)).ok().flatten()?;
    (flow.access)(&credentials).ok()?.base_url
}

/// Sign-out generation per provider. A sign-out advances it while holding the grant lock, so a
/// login that began earlier can no longer publish its grant.
static SIGN_OUT_GENERATIONS: Mutex<BTreeMap<&'static str, u64>> = Mutex::new(BTreeMap::new());

fn sign_out_generation(id: &'static str) -> u64 {
    SIGN_OUT_GENERATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(id)
        .copied()
        .unwrap_or(0)
}

fn advance_sign_out_generation(id: &'static str) {
    *SIGN_OUT_GENERATIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(id)
        .or_insert(0) += 1;
}

/// Run `id`'s interactive login and store the grant. The grant is not stored when the provider is
/// signed out while the login runs, including after the flow returns and before it saves.
pub fn login(
    user_dir: &Path,
    id: &str,
    ui: &dyn Interaction,
    cancel: Arc<AtomicBool>,
) -> Result<String> {
    let flow = require(id)?;
    let generation = sign_out_generation(flow.id);
    let credentials = (flow.login)(&Login { ui, cancel })?;
    credentials.validate()?;
    publish(user_dir, flow, generation, &credentials)
}

/// Store a finished login only if no sign-out happened since `generation` was read.
fn publish(
    user_dir: &Path,
    flow: &'static Flow,
    generation: u64,
    credentials: &Credentials,
) -> Result<String> {
    let path = path(user_dir, flow.id);
    let _lock = locked(&path)?;
    ensure!(
        sign_out_generation(flow.id) == generation,
        "{} sign-in was cancelled by sign-out; the grant was not saved",
        flow.name
    );
    save(&path, credentials)?;
    let who = credentials
        .email
        .as_deref()
        .map(|e| format!(" as {e}"))
        .unwrap_or_default();
    Ok(format!(
        "{} signed in{who}; choose a model in /models",
        flow.name
    ))
}

pub fn sign_out(user_dir: &Path, id: &str) -> Result<String> {
    let flow = require(id)?;
    let path = path(user_dir, id);
    let _lock = locked_for(&path, None, std::time::Duration::from_secs(2))?;
    advance_sign_out_generation(flow.id);
    if path.exists() {
        std::fs::remove_file(&path).context("could not remove the stored OAuth grant")?;
    }
    Ok(format!("Signed out of {}", flow.name))
}

/// Fresh request credentials, refreshing the stored grant under the lock when it is near expiry.
pub fn access(user_dir: &Path, id: &str, cancel: &Arc<AtomicBool>) -> Result<Access> {
    access_inner(user_dir, id, cancel, false)
}

/// Refresh unconditionally (after the provider rejected the current token), then shape access.
pub fn refreshed_access(user_dir: &Path, id: &str, cancel: &Arc<AtomicBool>) -> Result<Access> {
    access_inner(user_dir, id, cancel, true)
}

fn access_inner(
    user_dir: &Path,
    id: &str,
    cancel: &Arc<AtomicBool>,
    force: bool,
) -> Result<Access> {
    let flow = require(id)?;
    let path = path(user_dir, id);
    let _lock = locked_cancellable(&path, cancel)?;
    let mut credentials = load(&path)?.with_context(|| {
        format!(
            "not signed in to {}; sign in from /providers or run `dial login {id}`",
            flow.name
        )
    })?;
    if force || credentials.expired(REFRESH_MARGIN_MS) {
        match flow.refresh {
            Some(refresh) => {
                let mut fresh = refresh(&credentials, cancel).with_context(|| {
                    format!(
                        "{} sign-in could not be refreshed; sign in again from /providers",
                        flow.name
                    )
                })?;
                fresh.validate()?;
                if fresh.refresh.is_empty() {
                    fresh.refresh = std::mem::take(&mut credentials.refresh);
                }
                credentials = fresh;
                save(&path, &credentials)?;
            }
            None if credentials.expired(0) => {
                bail!(
                    "{} sign-in expired; sign in again from /providers",
                    flow.name
                )
            }
            None => {}
        }
    }
    (flow.access)(&credentials)
}

/// A provider route bound to a stored OAuth grant, resolved per request.
#[derive(Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: &'static str,
    pub user_dir: PathBuf,
}

impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Binding").field("id", &self.id).finish()
    }
}

impl Binding {
    /// Some when `provider` is an OAuth provider with a stored grant.
    pub fn for_provider(user_dir: &Path, provider: &str) -> Option<Self> {
        let flow = flow(provider)?;
        signed_in(user_dir, flow.id).then(|| Self {
            id: flow.id,
            user_dir: user_dir.to_path_buf(),
        })
    }
    pub fn flow(&self) -> &'static Flow {
        flow(self.id).expect("bindings are created from registered flows")
    }
    pub fn access(&self, cancel: &Arc<AtomicBool>) -> Result<Access> {
        access(&self.user_dir, self.id, cancel)
    }
    pub fn refreshed_access(&self, cancel: &Arc<AtomicBool>) -> Result<Access> {
        refreshed_access(&self.user_dir, self.id, cancel)
    }

    /// Refuse to attach this grant to a route it does not authorize. A route the grant supplies
    /// itself (`grant_routed`) is trusted; any other route must stay on the flow's canonical origin.
    /// The wire protocol must match the flow's in both cases.
    pub fn ensure_route(
        &self,
        grant_routed: bool,
        route: &str,
        protocol: ApiProtocol,
    ) -> Result<()> {
        let flow = self.flow();
        ensure!(
            protocol == flow.protocol,
            "{} sign-in is bound to its own protocol; this route uses another one",
            flow.name
        );
        ensure!(
            grant_routed || same_origin(route, flow.base_url),
            "{} sign-in is bound to its own endpoint and is not sent to other servers; use the provider's base URL or remove the custom one",
            flow.name
        );
        Ok(())
    }
}

/// Whether `route` and `canonical` are absolute URLs with the same scheme, host and port.
pub(crate) fn same_origin(route: &str, canonical: &str) -> bool {
    match (
        route.parse::<ureq::http::Uri>(),
        canonical.parse::<ureq::http::Uri>(),
    ) {
        (Ok(a), Ok(b)) => {
            a.host().is_some()
                && b.host().is_some()
                && crate::llm::catalog::origin(&a) == crate::llm::catalog::origin(&b)
        }
        _ => false,
    }
}

static NOTICE: Mutex<Option<String>> = Mutex::new(None);

/// The latest sign-in instruction posted by a background TUI login, once.
pub fn take_notice() -> Option<String> {
    NOTICE.lock().ok()?.take()
}

/// TUI interaction: instructions become a banner; prompts take their empty default when
/// allowed and otherwise direct the user to the terminal login command.
pub struct BannerInteraction {
    pub id: &'static str,
}

impl Interaction for BannerInteraction {
    fn auth(&self, url: &str, instructions: Option<&str>) {
        let text = match instructions {
            Some(instructions) => format!("{instructions} — at {url}"),
            None => format!("Continue sign-in in your browser: {url}"),
        };
        if let Ok(mut notice) = NOTICE.lock() {
            *notice = Some(text);
        }
    }
    fn progress(&self, message: &str) {
        if let Ok(mut notice) = NOTICE.lock() {
            *notice = Some(message.to_owned());
        }
    }
    fn prompt(
        &self,
        message: &str,
        _placeholder: Option<&str>,
        allow_empty: bool,
    ) -> Result<String> {
        if allow_empty {
            return Ok(String::new());
        }
        bail!(
            "{message}: this sign-in needs typed input; run `dial login {}` in a terminal",
            self.id
        )
    }
}

#[cfg(test)]
mod tests;

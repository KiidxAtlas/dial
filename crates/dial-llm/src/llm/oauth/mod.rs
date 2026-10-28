//! Provider OAuth sign-in.
//!
//! Each provider module exposes a [`Flow`]: an interactive login that returns [`Credentials`], an
//! optional refresh, and a request-time [`Access`] shaper. Each provider keeps one or more stored accounts (`oauth-<id>.json`, owner-only, cross-process
//! locked; see [`accounts`]). A session binds to one account and rotates to another after a rate
//! limit, quota, or auth failure. Each grant refreshes under its account's own lock immediately
//! before a request; nothing is written inside the workspace.
//!
//! Interaction is abstracted by [`Interaction`]: the TUI shows the verification URL/code as a
//! banner and opens the browser, while `dial login <provider>` can also prompt on the terminal.
use super::auth::SecretToken;
use super::errors::ErrorClass;
use super::model_config::ApiProtocol;
use crate::config::settings_store::{locked, locked_cancellable, locked_for, write_private};
use crate::llm::key_pool;
use accounts::Store;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, SystemTime},
};

pub mod accounts;
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
    &gitlab::AGENT_FLOW,
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

type Grants = Store<Credentials>;

/// Every account stored for a provider. A missing file holds none; a legacy single-grant file
/// loads as one account (see [`accounts::parse`]).
fn load(path: &Path) -> Result<Grants> {
    match crate::platform::atomic::read_private(path)? {
        Some(bytes) => accounts::parse(&bytes),
        None => Ok(Grants::default()),
    }
}

fn save(path: &Path, grants: &Grants) -> Result<()> {
    write_private(path, &serde_json::to_vec(grants)?)
}

pub fn signed_in(user_dir: &Path, id: &str) -> bool {
    flow(id).is_some() && load(&path(user_dir, id)).is_ok_and(|grants| !grants.is_empty())
}

/// Public identity of the active grant (email, organization or account id), for display.
pub fn identity(user_dir: &Path, id: &str) -> Option<String> {
    load(&path(user_dir, id)).ok()?.active().and_then(label)
}

/// Enterprise host recorded at login (Copilot GitHub Enterprise), when the active grant has one.
pub fn enterprise_url(user_dir: &Path, id: &str) -> Option<String> {
    load(&path(user_dir, id))
        .ok()?
        .active()?
        .enterprise_url
        .clone()
}

/// The endpoint a signed-in grant routes to by itself (a Cloudflare gateway or Snowflake account
/// built from its identifiers, a regional plan URL). Catalog rows and request routing both read it,
/// so they name the same route.
pub fn grant_route(user_dir: &Path, id: &str) -> Option<String> {
    let flow = flow(id)?;
    let grants = load(&path(user_dir, id)).ok()?;
    (flow.access)(grants.active()?).ok()?.base_url
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
    let mut grants = load(&path)?;
    let key = account_key(&grants, credentials);
    grants.insert(key.clone(), credentials.clone());
    grants.select(&key);
    save(&path, &grants)?;
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

/// Remove one stored account. The grant file is deleted together with its last account.
pub fn sign_out_account(user_dir: &Path, id: &str, key: &str) -> Result<String> {
    let flow = require(id)?;
    let path = path(user_dir, id);
    let _lock = locked_for(&path, None, std::time::Duration::from_secs(2))?;
    let mut grants = load(&path)?;
    let removed = grants
        .remove(key)
        .with_context(|| format!("no stored {} account to remove", flow.name))?;
    if grants.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path).context("could not remove the stored OAuth grant")?;
        }
    } else {
        save(&path, &grants)?;
    }
    let who = label(&removed).unwrap_or_else(|| key.to_owned());
    Ok(format!("Removed {} account {who}", flow.name))
}

/// One stored account as shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// Stable key (account id, email, or `account-N`) for [`select_account`] and [`sign_out_account`].
    pub key: String,
    pub label: String,
    pub active: bool,
    /// Recently rate-limited, quota-spent, or rejected; new sessions skip it until the cooldown ends.
    pub cooling: bool,
}

/// The stored accounts for `id`, in key order, with the active and cooling-down ones marked.
pub fn list_accounts(user_dir: &Path, id: &str) -> Result<Vec<AccountSummary>> {
    let flow = require(id)?;
    let grants = load(&path(user_dir, id))?;
    let active = grants.active_key().map(str::to_owned);
    let scope = accounts::scope(flow.id, user_dir);
    let blocks = key_pool::process_blocks();
    Ok(grants
        .accounts
        .iter()
        .map(|(key, credentials)| AccountSummary {
            key: key.clone(),
            label: label(credentials).unwrap_or_else(|| key.clone()),
            active: active.as_deref() == Some(key.as_str()),
            cooling: accounts::is_blocked(blocks, &scope, key),
        })
        .collect())
}

/// Stored accounts of every OAuth provider that has any, keyed by provider id. A store that cannot
/// be read counts as signed out, as in [`signed_in`].
pub fn accounts_by_provider(user_dir: &Path) -> BTreeMap<String, Vec<AccountSummary>> {
    flows()
        .filter_map(|flow| {
            let accounts = list_accounts(user_dir, flow.id).ok()?;
            (!accounts.is_empty()).then(|| (flow.id.to_owned(), accounts))
        })
        .collect()
}

/// Make `key` the account new sessions start on. Sessions already running keep their account.
pub fn select_account(user_dir: &Path, id: &str, key: &str) -> Result<String> {
    let flow = require(id)?;
    let path = path(user_dir, id);
    let _lock = locked_for(&path, None, std::time::Duration::from_secs(2))?;
    let mut grants = load(&path)?;
    ensure!(
        grants.select(key),
        "no stored {} account to select",
        flow.name
    );
    save(&path, &grants)?;
    Ok(format!("{} account selected", flow.name))
}

/// Fresh request credentials, refreshing the account under its own lock when it is near expiry.
pub fn access(user_dir: &Path, id: &str, cancel: &Arc<AtomicBool>) -> Result<Access> {
    access_inner(user_dir, id, None, cancel, false)
}

/// Refresh unconditionally (after the provider rejected the current token), then shape access.
pub fn refreshed_access(user_dir: &Path, id: &str, cancel: &Arc<AtomicBool>) -> Result<Access> {
    access_inner(user_dir, id, None, cancel, true)
}

fn access_inner(
    user_dir: &Path,
    id: &str,
    account: Option<&str>,
    cancel: &Arc<AtomicBool>,
    force: bool,
) -> Result<Access> {
    let flow = require(id)?;
    let path = path(user_dir, id);
    let grants = snapshot(&path, cancel)?;
    let key = match account {
        Some(key) => key.to_owned(),
        None => grants
            .active_key()
            .map(str::to_owned)
            .with_context(|| not_signed_in(flow))?,
    };
    let loaded = grants
        .accounts
        .get(&key)
        .cloned()
        .with_context(|| not_signed_in(flow))?;
    if !force && !loaded.expired(REFRESH_MARGIN_MS) {
        return (flow.access)(&loaded);
    }
    // Refresh under this account's own lock: a process that waited reloads the grant and reuses
    // the refresh the other process made, rather than spending the refresh token a second time.
    let _refresh = locked_cancellable(&accounts::refresh_lock_path(user_dir, id, &key), cancel)?;
    let mut credentials = snapshot(&path, cancel)?
        .accounts
        .get(&key)
        .cloned()
        .with_context(|| not_signed_in(flow))?;
    let already_refreshed = if force {
        credentials.access != loaded.access
    } else {
        !credentials.expired(REFRESH_MARGIN_MS)
    };
    if already_refreshed {
        return (flow.access)(&credentials);
    }
    let Some(refresh) = flow.refresh else {
        ensure!(
            !credentials.expired(0),
            "{} sign-in expired; sign in again from /providers",
            flow.name
        );
        return (flow.access)(&credentials);
    };
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
    store_grant(&path, &key, &fresh, cancel)?;
    (flow.access)(&fresh)
}

fn not_signed_in(flow: &Flow) -> String {
    format!(
        "not signed in to {}; sign in from /providers or run `dial login {}`",
        flow.name, flow.id
    )
}

/// The account store as it is on disk, read under its lock.
fn snapshot(path: &Path, cancel: &AtomicBool) -> Result<Grants> {
    let _lock = locked_cancellable(path, cancel)?;
    load(path)
}

/// Write one account's refreshed grant under the store lock. An account removed meanwhile is not
/// added back.
fn store_grant(
    path: &Path,
    key: &str,
    credentials: &Credentials,
    cancel: &AtomicBool,
) -> Result<()> {
    let _lock = locked_cancellable(path, cancel)?;
    let mut grants = load(path)?;
    if let Some(slot) = grants.accounts.get_mut(key) {
        *slot = credentials.clone();
        save(path, &grants)?;
    }
    Ok(())
}

/// The active account of a provider a new session binds to: the active one, or the next usable
/// one while it cools down.
fn pick_account(user_dir: &Path, id: &'static str) -> Option<String> {
    let grants = load(&path(user_dir, id)).ok()?;
    let active = grants.active_key()?.to_owned();
    let scope = accounts::scope(id, user_dir);
    let blocks = key_pool::process_blocks();
    if !accounts::is_blocked(blocks, &scope, &active) {
        return Some(active);
    }
    Some(
        accounts::next_account(&grants, Some(&active), |key| {
            accounts::is_blocked(blocks, &scope, key)
        })
        .unwrap_or(active),
    )
}

/// Stable key for a new grant: its account id, else its email, else the next free `account-N`.
/// Logging in again as the same account replaces that account's grant.
fn account_key(grants: &Grants, credentials: &Credentials) -> String {
    if let Some(key) = credentials
        .account_id
        .clone()
        .or_else(|| credentials.email.clone())
        .filter(|key| !key.is_empty())
    {
        return key;
    }
    let mut n = grants.accounts.len() + 1;
    while grants.accounts.contains_key(&format!("account-{n}")) {
        n += 1;
    }
    format!("account-{n}")
}

/// Display label of a grant: email, organization, or account id.
fn label(credentials: &Credentials) -> Option<String> {
    credentials
        .email
        .clone()
        .or_else(|| credentials.org_name.clone())
        .or_else(|| credentials.account_id.clone())
}

/// Resolve the credential route without reading credentials for an explicitly selected API key.
/// Explicit sign-in stays selected even when credentials are absent or need repair.
pub fn selected_auth(
    user_dir: &Path,
    provider: &str,
    method: super::model_config::AuthMethod,
) -> Result<bool> {
    use super::model_config::AuthMethod;
    match method {
        AuthMethod::ApiKey => {
            ensure!(
                provider != super::chatgpt::PROVIDER,
                "ChatGPT uses sign-in only; select openai for API-key billing"
            );
            Ok(false)
        }
        AuthMethod::OAuth => {
            ensure!(
                provider == "anthropic"
                    || provider == super::chatgpt::PROVIDER
                    || flow(provider).is_some(),
                "this provider has no sign-in route; select API-key authentication"
            );
            Ok(true)
        }
        AuthMethod::Auto if provider == "anthropic" => super::anthropic_oauth::signed_in_checked(
            &crate::config::paths::subscription_auth_dir(user_dir)?,
        ),
        AuthMethod::Auto if provider == super::chatgpt::PROVIDER => Ok(true),
        AuthMethod::Auto => Ok(flow(provider).is_some_and(|flow| signed_in(user_dir, flow.id))),
    }
}

/// A provider route bound to one stored OAuth account, resolved per request. The binding pins the
/// account for its session (sticky); [`Binding::rotate`] moves it after a rate limit, quota, or
/// auth failure.
#[derive(Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: &'static str,
    pub user_dir: PathBuf,
    /// The account this session uses. `None` when the provider has no stored account.
    pub account: Option<String>,
}

impl fmt::Debug for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Binding").field("id", &self.id).finish()
    }
}

impl Binding {
    /// Some when `provider` is an OAuth provider with a stored account.
    pub fn for_provider(user_dir: &Path, provider: &str) -> Option<Self> {
        let flow = flow(provider)?;
        let account = pick_account(user_dir, flow.id)?;
        Some(Self {
            id: flow.id,
            user_dir: user_dir.to_path_buf(),
            account: Some(account),
        })
    }
    /// Bind the explicitly selected sign-in, even if the user still needs to sign in.
    pub fn selected(user_dir: &Path, provider: &str) -> Option<Self> {
        let flow = flow(provider)?;
        Some(Self {
            id: flow.id,
            user_dir: user_dir.to_path_buf(),
            account: pick_account(user_dir, flow.id),
        })
    }
    pub fn flow(&self) -> &'static Flow {
        flow(self.id).expect("bindings are created from registered flows")
    }
    pub fn access(&self, cancel: &Arc<AtomicBool>) -> Result<Access> {
        access_inner(
            &self.user_dir,
            self.id,
            self.account.as_deref(),
            cancel,
            false,
        )
    }
    pub fn refreshed_access(&self, cancel: &Arc<AtomicBool>) -> Result<Access> {
        access_inner(
            &self.user_dir,
            self.id,
            self.account.as_deref(),
            cancel,
            true,
        )
    }

    /// Move this session to the next usable account after `class` failed on the current one. The
    /// failed account cools down ([`accounts::block_account`]); the switch lasts for this session.
    /// Returns false when `class` does not rotate or no other account is usable.
    pub fn rotate(&mut self, class: &ErrorClass, hinted: Option<Duration>) -> bool {
        let Some(current) = self.account.clone() else {
            return false;
        };
        let scope = accounts::scope(self.id, &self.user_dir);
        let blocks = key_pool::process_blocks();
        if !accounts::block_account(blocks, &scope, &current, class, hinted, SystemTime::now()) {
            return false;
        }
        let Ok(grants) = load(&path(&self.user_dir, self.id)) else {
            return false;
        };
        match accounts::next_account(&grants, Some(&current), |key| {
            accounts::is_blocked(blocks, &scope, key)
        }) {
            Some(next) => {
                self.account = Some(next);
                true
            }
            None => false,
        }
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

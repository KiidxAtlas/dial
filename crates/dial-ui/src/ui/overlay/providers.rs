//! Provider setup: every known endpoint with a colored status mark, per-provider actions (choose a
//! model, set/replace/remove the API key, enable/disable, remove) and a wizard to add a provider.
//!
//! API keys are typed into a masked field backed by [`Secret`]; the text is never drawn, never in
//! `Debug` output, and is moved into the single `SaveCredential` event (or wiped) the moment the
//! field is confirmed or abandoned. Whether a key is saved is asked of the controller through the
//! `has_key` callback, so the display always reflects the real credential store.
use super::{
    DrawCtx, OverlayEvent,
    settings::{
        editor_spans, filled, fit, is_ctrl, other_scope, pair_label, pressed, scope_name,
        scroll_cell, typed_char,
    },
};
use crate::{
    config::settings_store::Scope,
    llm::catalog::{Discovery, ProviderStatus, UNREACHABLE_DETAIL},
    llm::model_config::ProviderSpec,
    ui::{
        chrome::{self, Section, Status},
        editor::Editor,
        keys::{Action, Keymap},
        text,
        theme::{Sym, Theme, Tok},
    },
};
use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::{collections::BTreeMap, fmt};

/// A typed credential. It cannot be displayed or debug-printed, and its memory is overwritten when it
/// is wiped or dropped.
struct Secret(String);

impl Secret {
    fn new() -> Self {
        Secret(String::new())
    }
    fn len(&self) -> usize {
        self.0.chars().count()
    }
    fn push(&mut self, c: char) -> bool {
        if c.is_ascii_graphic() {
            self.0.push(c);
            true
        } else {
            false
        }
    }
    fn pop(&mut self) {
        self.0.pop();
    }
    /// Hand the text to its one legitimate consumer; nothing stays behind.
    fn take(&mut self) -> String {
        std::mem::take(&mut self.0)
    }
    fn wipe(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(<{} characters hidden>)", self.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Id,
    Label,
    Url,
    Env,
}

const STEPS: [Step; 4] = [Step::Id, Step::Label, Step::Url, Step::Env];

impl Step {
    fn title(self) -> &'static str {
        match self {
            Step::Id => "Provider id",
            Step::Label => "Display name (optional)",
            Step::Url => "Base URL",
            Step::Env => "API key environment variable (optional)",
        }
    }
    fn help(self) -> &'static str {
        match self {
            Step::Id => {
                "Letters, digits, '-' and '_' (1-64). Used in config and as provider/model."
            }
            Step::Label => "Shown in lists. Leave empty to use the id.",
            Step::Url => {
                "OpenAI-compatible endpoint, e.g. https://api.example.com/v1 or http://127.0.0.1:1234/v1"
            }
            Step::Env => {
                "Name of a variable holding the key, e.g. EXAMPLE_API_KEY. The key itself is never stored in config."
            }
        }
    }
}

struct Wizard {
    step: usize,
    values: [String; 4],
    editor: Editor,
    error: Option<String>,
}

impl Wizard {
    fn new() -> Self {
        Wizard {
            step: 0,
            values: Default::default(),
            editor: Editor::default(),
            error: None,
        }
    }
    fn current(&self) -> Step {
        STEPS[self.step]
    }
}

enum ProviderAction {
    ChooseModel,
    TestConnection,
    Headers,
    SetKey,
    OpenRouterLogin,
    AnthropicLogin,
    AnthropicSignOut,
    RemoveKey,
    Toggle,
    Remove,
    ChatGptLogin(Option<String>),
    ChatGptSelect(String),
    ChatGptSignOut(String),
    ChatGptCancel,
    OAuthLogin(String),
    OAuthSignOut(String),
}

struct Menu {
    entry: Entry,
    items: Vec<(ProviderAction, String)>,
    cursor: usize,
}

/// What a confirmed Confirm-mode action removes; nothing changes until the user confirms.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Removal {
    Provider,
    SavedKey,
    AnthropicSignOut,
}

impl Removal {
    fn title(self) -> &'static str {
        match self {
            Removal::Provider => "Remove provider",
            Removal::SavedKey => "Remove saved key",
            Removal::AnthropicSignOut => "Sign out",
        }
    }
}

enum Mode {
    List,
    Menu(Menu),
    Key {
        entry: Entry,
        secret: Secret,
        error: Option<String>,
    },
    Confirm(Entry, Removal),
    Wizard(Wizard),
    Headers {
        entry: Entry,
        editor: super::headers::HeaderEditor,
    },
}

#[derive(Clone)]
struct Entry {
    id: String,
    label: String,
    base_url: Option<String>,
    state: String,
    detail: String,
    models: usize,
    spec: Option<ProviderSpec>,
}

/// Whether a base URL names this machine: a loopback address or `localhost`.
fn is_loopback(base_url: &str) -> bool {
    url::Url::parse(base_url).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            bare.eq_ignore_ascii_case("localhost")
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    })
}

/// Whether an unavailable row is an expected absence, not a failure: a configured local server
/// that refused the connection is "not running", and so is a row Dial has not configured.
/// Auth failures, HTTP errors and malformed replies from a configured provider stay red.
fn expected_absent(entry: &Entry) -> bool {
    if entry.state != "unavailable" {
        return false;
    }
    entry.spec.is_none()
        || (entry.detail == UNREACHABLE_DETAIL
            && entry.base_url.as_deref().is_some_and(is_loopback))
}

/// Where a provider's API key comes from: the saved key in credentials.json and/or an
/// environment variable. The environment variable wins when it is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialSource {
    pub saved: bool,
    pub env: Option<EnvCredential>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvCredential {
    pub name: String,
    pub set: bool,
}

pub struct ProviderOverlay {
    chatgpt_pending: bool,
    chatgpt_accounts: Vec<crate::llm::chatgpt::AccountInfo>,
    anthropic_signed_in: bool,
    discovery: Option<Discovery>,
    configured: BTreeMap<String, ProviderSpec>,
    has_key: Box<dyn Fn(&str) -> bool>,
    scope: Scope,
    cursor: usize,
    scroll: usize,
    page: usize,
    mode: Mode,
    message: Option<(String, bool)>,
    /// A credential save awaits the controller's verdict; shown neutral until `set_status` answers.
    saving: bool,
    credential_sources: BTreeMap<String, CredentialSource>,
}

impl fmt::Debug for ProviderOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderOverlay")
            .field("scope", &self.scope)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_env(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The base URL Dial would accept: http(s), a host, no credentials/query/fragment, and plain http only
/// to local servers (the same rule the configuration applies).
fn validate_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("Base URL must start with http:// or https://".to_owned());
    }
    if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("Base URL must be a single line without spaces".to_owned());
    }
    if url.contains('#') {
        return Err("Base URL must not contain a fragment".to_owned());
    }
    crate::llm::provider::endpoint_url(url, false)
        .map(|_| url.to_owned())
        .map_err(|_| "Not accepted: use https, or http only for a local server; no credentials or query in the URL".to_owned())
}

impl ProviderOverlay {
    pub fn new(
        discovery: Option<Discovery>,
        configured: BTreeMap<String, ProviderSpec>,
        has_key: Box<dyn Fn(&str) -> bool>,
        scope: Scope,
    ) -> Self {
        ProviderOverlay {
            chatgpt_pending: false,
            chatgpt_accounts: Vec::new(),
            anthropic_signed_in: false,
            discovery,
            configured,
            has_key,
            scope,
            cursor: 0,
            scroll: 0,
            page: 8,
            mode: Mode::List,
            message: None,
            saving: false,
            credential_sources: BTreeMap::new(),
        }
    }

    pub fn set_chatgpt_accounts(&mut self, accounts: Vec<crate::llm::chatgpt::AccountInfo>) {
        self.chatgpt_accounts = accounts;
    }
    pub fn set_chatgpt_pending(&mut self, pending: bool) {
        self.chatgpt_pending = pending;
    }
    pub fn set_anthropic_signed_in(&mut self, signed_in: bool) {
        self.anthropic_signed_in = signed_in;
    }
    /// Where each provider's key comes from, keyed by base URL. The caller
    /// resolves the saved key and the environment variable; this overlay only describes them.
    pub fn set_credential_sources(&mut self, sources: BTreeMap<String, CredentialSource>) {
        self.credential_sources = sources;
    }

    pub fn set_discovery(&mut self, d: Discovery) {
        let anchor = self.entries().get(self.cursor).map(|e| e.id.clone());
        self.discovery = Some(d);
        let entries = self.entries();
        self.cursor = anchor
            .and_then(|id| entries.iter().position(|e| e.id == id))
            .unwrap_or_else(|| self.cursor.min(entries.len()));
    }

    /// Known providers (discovery order), then configured ones discovery has not reported yet.
    fn entries(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = Vec::new();
        let known: &[ProviderStatus] = self
            .discovery
            .as_ref()
            .map_or(&[], |d| d.providers.as_slice());
        for p in known {
            out.push(Entry {
                id: p.id.clone(),
                label: p.label.clone(),
                base_url: self
                    .configured
                    .get(&p.id)
                    .map(|spec| spec.base_url.clone())
                    .filter(|url| !url.is_empty())
                    .or_else(|| p.base_url.clone()),
                state: p.state.clone(),
                detail: p.detail.clone(),
                models: p.models,
                spec: self.configured.get(&p.id).cloned(),
            });
        }
        for (id, spec) in &self.configured {
            if out.iter().any(|e| &e.id == id) {
                continue;
            }
            let state = if !spec.enabled {
                "disabled"
            } else if self.discovery.is_some() {
                "unavailable"
            } else {
                "checking"
            };
            out.push(Entry {
                id: id.clone(),
                label: if spec.label.is_empty() {
                    id.clone()
                } else {
                    spec.label.clone()
                },
                base_url: Some(spec.base_url.clone()).filter(|u| !u.is_empty()),
                state: state.to_owned(),
                detail: if self.discovery.is_some() {
                    "Not reported by the last scan".to_owned()
                } else {
                    "Waiting for the first scan".to_owned()
                },
                models: 0,
                spec: Some(spec.clone()),
            });
        }
        // What you can use now first, then what you set up, then everything still to set up.
        out.sort_by_key(|entry| match entry.state.as_str() {
            "reachable" | "cached" => 0,
            _ if entry.spec.is_some() => 1,
            "auth-required" => 3,
            _ => 2,
        });
        out
    }

    /// Rows of the list: every entry plus the trailing "Add provider".
    fn row_count(&self) -> usize {
        self.entries().len() + 1
    }

    fn key_saved(&self, entry: &Entry) -> bool {
        entry
            .base_url
            .as_deref()
            .is_some_and(|url| (self.has_key)(url))
    }

    // ---- input ----------------------------------------------------------------------------

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        match event {
            Event::Key(key) if pressed(key) => self.key(key, keys),
            Event::Paste(pasted) => {
                match &mut self.mode {
                    Mode::Headers { editor, .. } => editor.paste(pasted),
                    Mode::Key { secret, error, .. } => {
                        let clean: String = pasted.chars().filter(|c| !c.is_whitespace()).collect();
                        if clean.chars().all(|c| c.is_ascii_graphic()) {
                            clean.chars().for_each(|c| {
                                secret.push(c);
                            });
                            *error = None;
                        } else {
                            *error = Some(
                                "Keys are printable ASCII without spaces; the paste was ignored"
                                    .to_owned(),
                            );
                        }
                    }
                    Mode::Wizard(w) => {
                        w.editor.insert(&text::one_line(pasted));
                        w.error = None;
                    }
                    _ => {}
                }
                OverlayEvent::None
            }
            _ => OverlayEvent::None,
        }
    }

    fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        match &self.mode {
            Mode::List => self.list_key(key, keys),
            Mode::Menu(_) => self.menu_key(key, keys),
            Mode::Key { .. } => self.secret_key(key, keys),
            Mode::Confirm(..) => self.confirm_key(key, keys),
            Mode::Wizard(_) => self.wizard_key(key, keys),
            Mode::Headers { .. } => self.headers_key(key, keys),
        }
    }

    /// One-line banner near the footer (success or error); cleared by the next key press.
    pub fn set_status(&mut self, message: String, is_error: bool) {
        self.message = Some((message, !is_error));
        self.saving = false;
    }

    fn list_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        self.message = None;
        self.saving = false;
        let count = self.row_count();
        let last = count - 1;
        if is_ctrl(key, 's') {
            self.scope = other_scope(self.scope);
            return OverlayEvent::None;
        }
        if key.code == KeyCode::F(5) || is_ctrl(key, 'r') {
            return OverlayEvent::Refresh;
        }
        if keys.matches(Action::SelectCancel, key) {
            return OverlayEvent::Close;
        }
        if keys.matches(Action::SelectUp, key) {
            self.cursor = if self.cursor == 0 {
                last
            } else {
                self.cursor - 1
            };
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = if self.cursor >= last {
                0
            } else {
                self.cursor + 1
            };
        } else if keys.matches(Action::SelectPageUp, key) {
            self.cursor = self.cursor.saturating_sub(self.page);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.cursor = (self.cursor + self.page).min(last);
        } else if keys.matches(Action::SelectConfirm, key) {
            self.open_menu();
        } else if key.code == KeyCode::Delete {
            return self.begin_remove();
        } else if let Some(c) = typed_char(key) {
            match c.to_ascii_lowercase() {
                'a' | '+' => self.mode = Mode::Wizard(Wizard::new()),
                'k' => self.begin_key(),
                'x' => return self.begin_remove(),
                _ => {}
            }
        }
        OverlayEvent::None
    }

    fn selected(&self) -> Option<Entry> {
        self.entries().get(self.cursor).cloned()
    }

    fn open_menu(&mut self) {
        let Some(entry) = self.selected() else {
            self.mode = Mode::Wizard(Wizard::new());
            return;
        };
        let saved = self.key_saved(&entry);
        let mut items = vec![(ProviderAction::ChooseModel, "Choose a model".to_owned())];
        if entry.id != crate::llm::chatgpt::PROVIDER && entry.base_url.is_some() {
            items.push((ProviderAction::TestConnection, "Test connection".to_owned()));
        }
        if entry.id == crate::llm::chatgpt::PROVIDER {
            if self.chatgpt_pending {
                items.push((
                    ProviderAction::ChatGptCancel,
                    "Cancel pending sign-in".into(),
                ));
            }
            items.push((
                ProviderAction::ChatGptLogin(None),
                "Continue with ChatGPT (add account)".into(),
            ));
            for account in &self.chatgpt_accounts {
                let label = text::one_line(&account.label);
                items.push((
                    ProviderAction::ChatGptLogin(Some(account.client_id.clone())),
                    format!("Sign in again: {label}"),
                ));
                if account.signed_in {
                    items.push((
                        ProviderAction::ChatGptSelect(account.client_id.clone()),
                        format!(
                            "{} {label}",
                            if account.active {
                                "Active:"
                            } else {
                                "Use account:"
                            }
                        ),
                    ));
                    items.push((
                        ProviderAction::ChatGptSignOut(account.client_id.clone()),
                        format!("Sign out: {label}"),
                    ));
                }
            }
            items.push((
                ProviderAction::Toggle,
                if entry.state == "disabled" {
                    "Enable provider"
                } else {
                    "Disable provider"
                }
                .into(),
            ));
        } else if entry.base_url.is_some() {
            items.push((
                ProviderAction::SetKey,
                if saved {
                    "Replace API key"
                } else {
                    "Set API key"
                }
                .to_owned(),
            ));
            if entry.id == "anthropic"
                && entry.base_url.as_deref() == Some(crate::llm::anthropic::BASE_URL)
            {
                items.push((
                    ProviderAction::AnthropicLogin,
                    if self.anthropic_signed_in {
                        "Sign in again with Claude subscription"
                    } else {
                        "Sign in with Claude subscription"
                    }
                    .into(),
                ));
                if self.anthropic_signed_in {
                    items.push((
                        ProviderAction::AnthropicSignOut,
                        "Sign out of Claude subscription".into(),
                    ));
                }
            }
            if entry.id == "openrouter"
                && entry.base_url.as_deref() == Some("https://openrouter.ai/api/v1")
            {
                items.push((
                    ProviderAction::OpenRouterLogin,
                    "Sign in with OpenRouter".to_owned(),
                ));
            }
            if let Some(flow) = crate::llm::oauth::flow(&entry.id)
                && !flow.needs_terminal
            {
                let signed_out = entry.state == "auth-required";
                items.push((
                    ProviderAction::OAuthLogin(flow.id.to_owned()),
                    if signed_out {
                        format!("Sign in with {}", flow.name)
                    } else {
                        format!("Sign in again with {}", flow.name)
                    },
                ));
                if !signed_out {
                    items.push((
                        ProviderAction::OAuthSignOut(flow.id.to_owned()),
                        format!("Sign out of {}", flow.name),
                    ));
                }
            }
            if saved {
                items.push((
                    ProviderAction::RemoveKey,
                    if entry.id == "anthropic" {
                        "Remove saved credential / sign out"
                    } else {
                        "Remove API key"
                    }
                    .to_owned(),
                ));
            }
            items.push((
                ProviderAction::Toggle,
                if entry.state == "disabled" {
                    "Enable provider"
                } else {
                    "Disable provider"
                }
                .to_owned(),
            ));
            if entry.id != "anthropic" || !self.anthropic_signed_in {
                items.push((ProviderAction::Headers, "Edit HTTP header rows".into()));
            }
        }
        if entry.spec.is_some() {
            items.push((ProviderAction::Remove, "Remove provider".to_owned()));
        }
        self.mode = Mode::Menu(Menu {
            entry,
            items,
            cursor: 0,
        });
    }

    fn begin_key(&mut self) {
        match self.selected() {
            Some(entry) if entry.id == crate::llm::chatgpt::PROVIDER => {
                self.message = Some((
                    "Use Continue with ChatGPT in this provider's menu".into(),
                    false,
                ))
            }
            Some(entry) if entry.base_url.is_some() => {
                self.mode = Mode::Key {
                    entry,
                    secret: Secret::new(),
                    error: None,
                }
            }
            Some(entry) => {
                self.message = Some((
                    format!(
                        "{} is a local provider and has no API key",
                        text::one_line(&entry.label)
                    ),
                    false,
                ))
            }
            None => {}
        }
    }

    fn begin_remove(&mut self) -> OverlayEvent {
        match self.selected() {
            Some(entry) if entry.spec.is_some() => {
                self.mode = Mode::Confirm(entry, Removal::Provider)
            }
            Some(entry) => {
                self.message = Some((
                    format!(
                        "{} is built in and is not in your config; disable it instead",
                        text::one_line(&entry.label)
                    ),
                    false,
                ))
            }
            None => {}
        }
        OverlayEvent::None
    }

    fn menu_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Menu(menu) = &mut self.mode else {
            return OverlayEvent::None;
        };
        let last = menu.items.len() - 1;
        if keys.matches(Action::SelectCancel, key) {
            self.mode = Mode::List;
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectUp, key) {
            menu.cursor = if menu.cursor == 0 {
                last
            } else {
                menu.cursor - 1
            };
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectDown, key) {
            menu.cursor = if menu.cursor >= last {
                0
            } else {
                menu.cursor + 1
            };
            return OverlayEvent::None;
        }
        if !keys.matches(Action::SelectConfirm, key) {
            return OverlayEvent::None;
        }
        let entry = menu.entry.clone();
        let action = match &menu.items[menu.cursor].0 {
            ProviderAction::Headers => {
                let headers = entry.spec.as_ref().and_then(|spec| spec.headers.clone());
                self.mode = Mode::Headers {
                    entry,
                    editor: super::headers::HeaderEditor::new(&headers),
                };
                return OverlayEvent::None;
            }
            ProviderAction::ChatGptLogin(id) => {
                let client_id = id.clone();
                self.mode = Mode::List;
                self.message = Some((
                    "Waiting for ChatGPT sign-in in your browser; you can close this panel".into(),
                    true,
                ));
                return OverlayEvent::ChatGptLogin { client_id };
            }
            ProviderAction::ChatGptCancel => {
                self.mode = Mode::List;
                return OverlayEvent::ChatGptCancel;
            }
            ProviderAction::ChatGptSelect(id) => {
                let client_id = id.clone();
                self.mode = Mode::List;
                return OverlayEvent::ChatGptSelect { client_id };
            }
            ProviderAction::ChatGptSignOut(id) => {
                let client_id = id.clone();
                self.mode = Mode::List;
                return OverlayEvent::ChatGptSignOut { client_id };
            }
            ProviderAction::AnthropicLogin => {
                self.mode = Mode::List;
                self.message = Some((
                    "Waiting for Claude subscription sign-in in your browser".into(),
                    true,
                ));
                return OverlayEvent::AnthropicLogin;
            }
            ProviderAction::AnthropicSignOut => {
                self.mode = Mode::Confirm(entry, Removal::AnthropicSignOut);
                return OverlayEvent::None;
            }
            ProviderAction::OAuthLogin(id) => {
                let id = id.clone();
                self.mode = Mode::List;
                let name = crate::llm::oauth::flow(&id).map_or(id.as_str(), |flow| flow.name);
                self.message = Some((format!("Waiting for {name} sign-in in your browser"), true));
                return OverlayEvent::OAuthLogin { id };
            }
            ProviderAction::OAuthSignOut(id) => {
                let id = id.clone();
                self.mode = Mode::List;
                return OverlayEvent::OAuthSignOut { id };
            }
            ProviderAction::ChooseModel => {
                self.mode = Mode::List;
                return OverlayEvent::Action("open:models".to_owned());
            }
            ProviderAction::TestConnection => {
                self.mode = Mode::List;
                self.message = Some((
                    format!("Testing connection to {}", text::one_line(&entry.label)),
                    true,
                ));
                return OverlayEvent::Action(format!("test:{}", entry.id));
            }
            ProviderAction::SetKey => {
                self.mode = Mode::Key {
                    entry,
                    secret: Secret::new(),
                    error: None,
                };
                return OverlayEvent::None;
            }
            ProviderAction::OpenRouterLogin => {
                self.mode = Mode::List;
                self.message = Some(("Starting OpenRouter browser sign-in".to_owned(), true));
                return OverlayEvent::OpenRouterLogin;
            }
            ProviderAction::RemoveKey => {
                if entry.base_url.is_none() {
                    return OverlayEvent::None;
                }
                self.mode = Mode::Confirm(entry, Removal::SavedKey);
                return OverlayEvent::None;
            }
            ProviderAction::Remove => {
                self.mode = Mode::Confirm(entry, Removal::Provider);
                return OverlayEvent::None;
            }
            ProviderAction::Toggle => entry,
        };
        self.mode = Mode::List;
        // Toggling keeps every other field of the provider and flips only `enabled`.
        let mut spec = action.spec.clone().unwrap_or_else(|| ProviderSpec {
            label: action.label.clone(),
            base_url: action.base_url.clone().unwrap_or_default(),
            ..ProviderSpec::default()
        });
        spec.enabled = action.state == "disabled";
        OverlayEvent::AddProvider {
            id: action.id,
            spec,
            scope: self.scope,
        }
    }

    fn headers_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Headers { entry, editor } = &mut self.mode else {
            return OverlayEvent::None;
        };
        match editor.key(key, keys) {
            super::headers::Outcome::Pending => OverlayEvent::None,
            super::headers::Outcome::Cancel => {
                self.mode = Mode::List;
                OverlayEvent::None
            }
            super::headers::Outcome::Save(headers) => {
                let mut spec = entry.spec.clone().unwrap_or_else(|| ProviderSpec {
                    label: entry.label.clone(),
                    base_url: entry.base_url.clone().unwrap_or_default(),
                    protocol: Some(crate::llm::model_config::ApiProtocol::builtin(&entry.id)),
                    ..Default::default()
                });
                spec.headers = headers;
                let id = entry.id.clone();
                self.mode = Mode::List;
                OverlayEvent::AddProvider {
                    id,
                    spec,
                    scope: self.scope,
                }
            }
        }
    }

    fn secret_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Key {
            entry,
            secret,
            error,
        } = &mut self.mode
        else {
            return OverlayEvent::None;
        };
        if keys.matches(Action::SelectCancel, key) {
            secret.wipe();
            self.mode = Mode::List;
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectConfirm, key) {
            if secret.len() == 0 {
                *error = Some("Type or paste the key first".to_owned());
                return OverlayEvent::None;
            }
            let Some(base_url) = entry.base_url.clone() else {
                return OverlayEvent::None;
            };
            let token = secret.take();
            let provider = entry.id.clone();
            let label = text::one_line(&entry.label);
            let message = if matches!(provider.as_str(), "openai" | "anthropic") {
                format!("Verifying {label}…")
            } else {
                format!("Saving {label} key…")
            };
            self.mode = Mode::List;
            // Neutral until the controller's reply reaches `set_status`.
            self.message = Some((message, true));
            self.saving = true;
            return OverlayEvent::SaveCredential {
                provider,
                base_url,
                token,
            };
        }
        if key.code == KeyCode::Backspace {
            secret.pop();
            *error = None;
        } else if is_ctrl(key, 'u') {
            secret.wipe();
            *error = None;
        } else if let Some(c) = typed_char(key) {
            *error = if secret.push(c) {
                None
            } else {
                Some("Keys are printable ASCII without spaces".to_owned())
            };
        }
        OverlayEvent::None
    }

    fn confirm_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Confirm(entry, removal) = &self.mode else {
            return OverlayEvent::None;
        };
        if keys.matches(Action::SelectConfirm, key) {
            let (id, base_url, removal) = (entry.id.clone(), entry.base_url.clone(), *removal);
            self.mode = Mode::List;
            return match removal {
                Removal::Provider => OverlayEvent::RemoveProvider {
                    id,
                    scope: self.scope,
                },
                Removal::SavedKey => match base_url {
                    Some(base_url) => {
                        self.message = Some(("API key removal requested".to_owned(), true));
                        OverlayEvent::RemoveCredential { base_url }
                    }
                    None => OverlayEvent::None,
                },
                Removal::AnthropicSignOut => OverlayEvent::AnthropicSignOut,
            };
        }
        if keys.matches(Action::SelectCancel, key) {
            self.mode = Mode::List;
        }
        OverlayEvent::None
    }

    /// Names what the confirmed action removes; nothing changes until Enter.
    fn confirm_body(
        &self,
        theme: &Theme,
        entry: &Entry,
        removal: Removal,
        w: usize,
    ) -> Vec<Line<'static>> {
        let label = text::one_line(&entry.label);
        let (main, note) = match removal {
            Removal::Provider => (
                format!(
                    "  Remove provider {} ({}) from the {} config?",
                    label,
                    text::one_line(&entry.id),
                    scope_name(self.scope)
                ),
                "  Its saved API key, if any, is kept until you remove it.".to_owned(),
            ),
            Removal::SavedKey if entry.id == "anthropic" => (
                "  Remove the saved credential and sign out of the Claude subscription?".to_owned(),
                "  Dial stops using the saved key and the subscription login.".to_owned(),
            ),
            Removal::SavedKey => (
                format!("  Remove the saved API key for {label}?"),
                "  Dial stops using that key; an environment variable, if set, still applies."
                    .to_owned(),
            ),
            Removal::AnthropicSignOut => (
                "  Sign out of the Claude subscription?".to_owned(),
                "  Dial stops using the subscription login; a saved API key is kept.".to_owned(),
            ),
        };
        vec![
            Line::default(),
            Line::from(fit(vec![Span::styled(main, theme.fg(Tok::Warning))], w)),
            Line::from(fit(vec![Span::styled(note, theme.fg(Tok::Muted))], w)),
        ]
    }

    fn wizard_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let configured: Vec<String> = self.configured.keys().cloned().collect();
        let scope = self.scope;
        let Mode::Wizard(w) = &mut self.mode else {
            return OverlayEvent::None;
        };
        if keys.matches(Action::SelectCancel, key) {
            if w.step == 0 {
                self.mode = Mode::List;
            } else {
                w.step -= 1;
                let previous = w.values[w.step].clone();
                w.editor.set(&previous);
                w.error = None;
            }
            return OverlayEvent::None;
        }
        if !keys.matches(Action::SelectConfirm, key) {
            if w.editor.key(*key) {
                w.error = None;
            }
            return OverlayEvent::None;
        }
        let input = w.editor.text.trim().to_owned();
        let checked: Result<String, String> = match w.current() {
            Step::Id if !valid_id(&input) => Err("Use 1-64 letters, digits, '-' or '_'".to_owned()),
            Step::Id if input.eq_ignore_ascii_case("mlx") => {
                Err("\"mlx\" is reserved for the local provider".to_owned())
            }
            Step::Id if configured.contains(&input) => Err(format!(
                "{input} is already configured; remove it first or choose another id"
            )),
            Step::Label if input.chars().count() > 64 => {
                Err("Keep the name to 64 characters".to_owned())
            }
            Step::Url => validate_url(&input),
            Step::Env if !input.is_empty() && !valid_env(&input) => {
                Err("Use letters, digits and '_' and do not start with a digit".to_owned())
            }
            _ => Ok(input),
        };
        match checked {
            Err(message) => w.error = Some(message),
            Ok(value) => {
                w.values[w.step] = value;
                w.error = None;
                if w.step + 1 < STEPS.len() {
                    w.step += 1;
                    let next = w.values[w.step].clone();
                    w.editor.set(&next);
                } else {
                    let [id, label, base_url, env] = std::mem::take(&mut w.values);
                    let spec = ProviderSpec {
                        label: if label.is_empty() { id.clone() } else { label },
                        base_url,
                        api_key_env: Some(env).filter(|e| !e.is_empty()),
                        enabled: true,
                        allow_insecure: false,
                        protocol: None,
                        headers: None,
                    };
                    self.mode = Mode::List;
                    self.message = Some((format!("Provider {id} sent for saving"), true));
                    return OverlayEvent::AddProvider { id, spec, scope };
                }
            }
        }
        OverlayEvent::None
    }

    // ---- drawing --------------------------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let extra = format!(
            "{} scope: {}",
            ctx.theme.sym(Sym::Dot),
            scope_name(self.scope)
        );
        let area = frame.area();
        if area.width < 5 || area.height < 5 {
            // Too small for a frame: the body alone.
            let mut lines = self.compose(area.width, area.height, ctx);
            lines.truncate(usize::from(area.height));
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }
        let block = chrome::panel_block_titled(ctx.theme, Section::Connect, "Providers", &extra);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.compose(inner.width, inner.height, ctx);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn compose(&mut self, w: u16, h: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(w), usize::from(h));
        let theme = ctx.theme;
        let body_min = h.min(3);
        let mut spare = h - body_min;
        let mut grant = || {
            if spare > 0 {
                spare -= 1;
                true
            } else {
                false
            }
        };
        let (show_footer, show_detail, show_message, show_header) =
            (grant(), grant(), grant(), grant());
        let body_rows = h - [show_footer, show_detail, show_message, show_header]
            .iter()
            .filter(|b| **b)
            .count();
        let mut out: Vec<Line<'static>> = Vec::with_capacity(h);
        if show_header {
            out.push(Line::from(fit(vec![Span::styled(
                format!(" {}", match &self.mode {
                    Mode::List => "Providers Dial can use. Enter opens the actions for the highlighted one.",
                    Mode::Menu(_) => "Actions",
                    Mode::Key { .. } => "API key",
                    Mode::Confirm(_, removal) => removal.title(),
                    Mode::Wizard(_) => "Add provider",
                    Mode::Headers { .. } => "HTTP header rows",
                }),
                theme.fg(Tok::Muted))], w)));
        }
        let entries = self.entries();
        let body: Vec<Line<'static>> = match &self.mode {
            Mode::List => self.list_body(theme, &entries, body_rows, w),
            Mode::Menu(menu) => self.menu_body(theme, menu, w, body_rows),
            Mode::Key {
                entry,
                secret,
                error,
            } => self.key_body(theme, entry, secret, error.as_deref(), w),
            Mode::Confirm(entry, removal) => self.confirm_body(theme, entry, *removal, w),
            Mode::Wizard(wizard) => self.wizard_body(theme, wizard, w),
            Mode::Headers { editor, .. } => editor.lines(theme, w, body_rows),
        };
        for y in 0..body_rows {
            out.push(body.get(y).cloned().unwrap_or_default());
        }
        if show_message {
            out.push(match &self.message {
                Some((message, ok)) => {
                    let (glyph, tone) = if self.saving {
                        (Sym::Working, Tok::Muted)
                    } else if *ok {
                        (Sym::Check, Tok::Success)
                    } else {
                        (Sym::Cross2, Tok::Error)
                    };
                    Line::from(fit(
                        vec![Span::styled(
                            format!(" {} {}", theme.sym(glyph), text::one_line(message)),
                            theme.fg(tone),
                        )],
                        w,
                    ))
                }
                None => Line::default(),
            });
        }
        if show_detail {
            out.push(Line::from(fit(self.detail(theme, &entries), w)));
        }
        if show_footer {
            out.push(key_footer(theme, &self.footer(ctx.keys), w));
        }
        out
    }

    fn chip(&self, theme: &Theme, state: &str) -> Span<'static> {
        let status = match state {
            "reachable" | "cached" => Status::Ready,
            // Not configured yet is a neutral state, not a warning.
            "auth-required" => return chrome::status_chip(theme, Status::Missing, "not set up"),
            "disabled" => Status::Missing,
            "checking" => {
                return Span::styled(
                    format!("{} {state}", theme.sym(Sym::Ellipsis)),
                    theme.fg(Tok::Muted),
                );
            }
            _ => Status::Error,
        };
        chrome::status_chip(theme, status, state)
    }

    fn list_body(
        &mut self,
        theme: &Theme,
        entries: &[Entry],
        rows: usize,
        w: usize,
    ) -> Vec<Line<'static>> {
        let total = entries.len() + 1;
        self.cursor = self.cursor.min(total - 1);
        self.page = rows.saturating_sub(1).max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.cursor >= self.scroll + rows {
            self.scroll = self.cursor + 1 - rows.max(1);
        }
        self.scroll = self.scroll.min(total.saturating_sub(rows));
        let overflow = total > rows;
        let list_w = w.saturating_sub(usize::from(overflow));
        let label_w = (list_w / 3).clamp(8, 26);
        let chip_w = 16;
        let mut lines = Vec::new();
        for y in 0..rows.min(total) {
            let index = self.scroll + y;
            let selected = index == self.cursor;
            let fill = if selected {
                theme.bg(Tok::SelectedBg)
            } else {
                Style::default()
            };
            let pointer = if selected {
                theme.sym(Sym::Pointer)
            } else {
                " "
            };
            let mut spans = vec![Span::styled(
                format!("{pointer} "),
                theme.fg(Tok::Accent).patch(fill),
            )];
            if let Some(entry) = entries.get(index) {
                // An unavailable row whose absence is expected is muted, not an error.
                let chip = if expected_absent(entry) {
                    chrome::status_chip(theme, Status::Missing, "not running")
                } else {
                    self.chip(theme, &entry.state)
                };
                let label =
                    text::ellipsize(&text::one_line(&entry.label), label_w - 1, theme.ascii());
                let style = theme.fg(Tok::Text).patch(fill);
                spans.push(Span::styled(
                    text::pad(&label, label_w),
                    if selected {
                        style.add_modifier(Modifier::BOLD)
                    } else {
                        style
                    },
                ));
                spans.push(Span::styled(
                    text::pad(
                        &text::ellipsize(&chip.content, chip_w - 1, theme.ascii()),
                        chip_w,
                    ),
                    chip.style.patch(fill),
                ));
                let count = if entry.models == 0 {
                    String::new()
                } else {
                    format!(
                        "{} {}",
                        entry.models,
                        if entry.models == 1 { "model" } else { "models" }
                    )
                };
                spans.push(Span::styled(
                    text::pad(&count, 11),
                    theme.fg(Tok::Muted).patch(fill),
                ));
                let (key, ktok) = self.key_status(theme, entry);
                spans.push(Span::styled(key, theme.fg(ktok).patch(fill)));
                if let Some(action) = self.next_action(entry) {
                    spans.push(Span::styled(
                        format!("  → {action}"),
                        theme
                            .fg(Tok::Accent)
                            .add_modifier(Modifier::BOLD)
                            .patch(fill),
                    ));
                }
            } else {
                spans.push(Span::styled(
                    "+ Add provider".to_owned(),
                    theme
                        .fg(Tok::Accent)
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        })
                        .patch(fill),
                ));
            }
            if selected {
                chrome::select_spans(theme, &mut spans);
            }
            let mut line = filled(spans, list_w, fill);
            if overflow {
                line.spans
                    .push(scroll_cell(theme, y, rows, total, self.scroll));
            }
            lines.push(line);
        }
        lines
    }

    fn key_status(&self, theme: &Theme, entry: &Entry) -> (String, Tok) {
        if entry.id == crate::llm::chatgpt::PROVIDER {
            return if self
                .chatgpt_accounts
                .iter()
                .any(|a| a.active && a.signed_in)
            {
                ("account signed in".into(), Tok::Success)
            } else {
                ("sign-in needed".into(), Tok::Warning)
            };
        }
        if entry.id == "anthropic" && self.anthropic_signed_in {
            return (
                format!("{} Claude subscription signed in", theme.sym(Sym::Check)),
                Tok::Success,
            );
        }
        if entry.base_url.is_none() {
            return (String::new(), Tok::Dim);
        }
        if self.key_saved(entry) {
            return (format!("{} key saved", theme.sym(Sym::Check)), Tok::Success);
        }
        if let Some(var) = entry.spec.as_ref().and_then(|s| s.api_key_env.as_deref()) {
            return (format!("env {}", text::one_line(var)), Tok::Muted);
        }
        if entry.state == "auth-required" {
            return ("key needed".to_owned(), Tok::Muted);
        }
        ("no key".to_owned(), Tok::Dim)
    }

    /// The single thing the user can do next for this row, if any. Pure over the row state.
    fn next_action(&self, entry: &Entry) -> Option<&'static str> {
        if entry.id == crate::llm::chatgpt::PROVIDER {
            let signed_in = self
                .chatgpt_accounts
                .iter()
                .any(|a| a.active && a.signed_in);
            return (!signed_in).then_some("Sign in");
        }
        let base_url = entry.base_url.as_deref()?;
        if entry.id == "anthropic" && self.anthropic_signed_in {
            return None;
        }
        match entry.state.as_str() {
            "auth-required" if !self.key_saved(entry) => Some("Add API key"),
            "unavailable"
                if ["127.0.0.1", "localhost", "[::1]"]
                    .iter()
                    .any(|host| base_url.contains(host)) =>
            {
                Some("Start the server")
            }
            _ => None,
        }
    }

    fn menu_body(&self, theme: &Theme, menu: &Menu, w: usize, rows: usize) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(fit(
            vec![Span::styled(
                format!("  {}", text::one_line(&menu.entry.label)),
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
            )],
            w,
        ))];
        let visible = rows.saturating_sub(1).max(1);
        let start = menu.cursor.saturating_sub(visible - 1);
        for (i, (_, label)) in menu.items.iter().enumerate().skip(start).take(visible) {
            let selected = i == menu.cursor;
            let fill = if selected {
                theme.bg(Tok::SelectedBg)
            } else {
                Style::default()
            };
            let radio = if selected {
                theme.sym(Sym::RadioOn)
            } else {
                theme.sym(Sym::RadioOff)
            };
            lines.push(filled(
                vec![
                    Span::styled(format!("    {radio} "), theme.fg(Tok::Accent).patch(fill)),
                    Span::styled(label.clone(), theme.fg(Tok::Text).patch(fill)),
                ],
                w,
                fill,
            ));
        }
        lines
    }

    fn key_body(
        &self,
        theme: &Theme,
        entry: &Entry,
        secret: &Secret,
        error: Option<&str>,
        w: usize,
    ) -> Vec<Line<'static>> {
        let dots = secret.len();
        let room = w.saturating_sub(8);
        let shown = if dots > room {
            format!(
                "{}{}",
                theme.sym(Sym::Ellipsis),
                "•".repeat(room.saturating_sub(1))
            )
        } else {
            "•".repeat(dots)
        };
        let mut lines = vec![
            Line::default(),
            Line::from(fit(
                vec![Span::styled(
                    format!("  API key for {}", text::one_line(&entry.label)),
                    theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
                )],
                w,
            )),
            Line::from(fit(
                vec![Span::styled(
                    format!(
                        "  stored for {}",
                        text::one_line(entry.base_url.as_deref().unwrap_or(""))
                    ),
                    theme.fg(Tok::Muted),
                )],
                w,
            )),
            Line::default(),
            Line::from(fit(
                vec![
                    Span::styled(
                        format!("  {} ", theme.sym(Sym::Prompt)),
                        theme.fg(Tok::Accent),
                    ),
                    Span::styled(shown, theme.fg(Tok::Text)),
                    Span::styled(
                        " ".to_owned(),
                        theme.fg(Tok::Text).add_modifier(Modifier::REVERSED),
                    ),
                ],
                w,
            )),
            Line::from(fit(
                vec![Span::styled(
                    format!(
                        "  {dots} characters; the key is hidden and is not kept after you press Enter"
                    ),
                    theme.fg(Tok::Dim),
                )],
                w,
            )),
        ];
        if let Some(error) = error {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    format!("  {} {error}", theme.sym(Sym::Cross2)),
                    theme.fg(Tok::Error),
                )],
                w,
            )));
        }
        lines
    }

    fn wizard_body(&self, theme: &Theme, wizard: &Wizard, w: usize) -> Vec<Line<'static>> {
        let mut lines = vec![Line::default()];
        for (i, step) in STEPS.iter().enumerate() {
            let done = i < wizard.step;
            let active = i == wizard.step;
            let mark = if done {
                theme.sym(Sym::Check)
            } else if active {
                theme.sym(Sym::Pointer)
            } else {
                " "
            };
            let tok = if active {
                Tok::Accent
            } else if done {
                Tok::Success
            } else {
                Tok::Dim
            };
            let mut spans = vec![Span::styled(
                format!("  {mark} {}", step.title()),
                theme.fg(tok).add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            )];
            if done {
                let value = if wizard.values[i].is_empty() {
                    "(none)".to_owned()
                } else {
                    text::one_line(&wizard.values[i])
                };
                spans.push(Span::styled(format!("  {value}"), theme.fg(Tok::Muted)));
            }
            if active {
                let mut input = vec![Span::styled("    [".to_owned(), theme.fg(Tok::Muted))];
                input.extend(editor_spans(
                    theme,
                    &wizard.editor,
                    w.saturating_sub(7),
                    false,
                ));
                input.push(Span::styled("]".to_owned(), theme.fg(Tok::Muted)));
                lines.push(Line::from(fit(spans, w)));
                lines.push(Line::from(fit(input, w)));
                lines.push(Line::from(fit(
                    vec![Span::styled(
                        format!("    {}", step.help()),
                        theme.fg(Tok::Dim),
                    )],
                    w,
                )));
                if let Some(error) = &wizard.error {
                    lines.push(Line::from(fit(
                        vec![Span::styled(
                            format!("    {} {error}", theme.sym(Sym::Cross2)),
                            theme.fg(Tok::Error),
                        )],
                        w,
                    )));
                }
            } else {
                lines.push(Line::from(fit(spans, w)));
            }
        }
        lines
    }

    fn detail(&self, theme: &Theme, entries: &[Entry]) -> Vec<Span<'static>> {
        if !matches!(self.mode, Mode::List) {
            return Vec::new();
        }
        match entries.get(self.cursor) {
            Some(entry) => {
                let summary = self.key_source_sentence(entry).unwrap_or_else(|| text::one_line(&entry.detail));
                let mut spans = vec![Span::styled(format!(" {summary}"), theme.fg(Tok::Muted))];
                if let Some(url) = &entry.base_url { spans.push(Span::styled(format!("  {} {}", theme.sym(Sym::Dot), text::one_line(url)), theme.fg(Tok::Dim))); }
                spans
            }
            None => vec![Span::styled(" Add an OpenAI-compatible endpoint: id, name, base URL and an optional key variable.".to_owned(), theme.fg(Tok::Muted))],
        }
    }

    /// One plain sentence on where this provider's key comes from: the saved key and the environment variable.
    fn key_source_sentence(&self, entry: &Entry) -> Option<String> {
        let source = self.credential_sources.get(entry.base_url.as_ref()?)?;
        let env = source
            .env
            .as_ref()
            .map(|e| (text::one_line(&e.name), e.set));
        Some(match (env, source.saved) {
            (Some((name, true)), true) => {
                format!("Key: from env {name} (takes precedence over the saved key)")
            }
            (Some((name, true)), false) => format!("Key: from env {name} (set)"),
            (Some((name, false)), true) => {
                format!("Key: saved in credentials.json; env {name} is not set")
            }
            (Some((name, false)), false) => format!("Key: env {name} is not set"),
            (None, true) => "Key: saved in credentials.json".to_owned(),
            (None, false) => "Key: none saved".to_owned(),
        })
    }

    fn footer(&self, keys: &Keymap) -> Vec<(String, String)> {
        let hint = |action: Action, what: &str| (keys.label(action), what.to_owned());
        match &self.mode {
            Mode::List => vec![
                hint(Action::SelectConfirm, "actions"),
                (
                    pair_label(keys, Action::SelectUp, Action::SelectDown),
                    "move".to_owned(),
                ),
                ("a".to_owned(), "add".to_owned()),
                ("k".to_owned(), "key".to_owned()),
                ("x".to_owned(), "remove".to_owned()),
                ("F5".to_owned(), "rescan".to_owned()),
                ("Ctrl+S".to_owned(), "scope".to_owned()),
                hint(Action::SelectCancel, "close"),
            ],
            Mode::Menu(_) => vec![
                hint(Action::SelectConfirm, "run"),
                (
                    pair_label(keys, Action::SelectUp, Action::SelectDown),
                    "move".to_owned(),
                ),
                hint(Action::SelectCancel, "back"),
            ],
            Mode::Key { .. } => vec![
                hint(Action::SelectConfirm, "save key"),
                ("Ctrl+U".to_owned(), "clear".to_owned()),
                hint(Action::SelectCancel, "cancel"),
            ],
            Mode::Headers { .. } => vec![
                ("F10".to_owned(), "save".to_owned()),
                hint(Action::SelectCancel, "cancel"),
            ],
            Mode::Confirm(_, removal) => vec![
                hint(
                    Action::SelectConfirm,
                    if matches!(removal, Removal::AnthropicSignOut) {
                        "sign out"
                    } else {
                        "remove"
                    },
                ),
                hint(Action::SelectConfirm, "remove"),
                hint(Action::SelectCancel, "cancel"),
            ],
            Mode::Wizard(w) => vec![
                hint(
                    Action::SelectConfirm,
                    if w.step + 1 == STEPS.len() {
                        "add provider"
                    } else {
                        "next"
                    },
                ),
                hint(
                    Action::SelectCancel,
                    if w.step == 0 { "cancel" } else { "back" },
                ),
            ],
        }
    }
}

/// Footer key hints (`key label` pairs) on one line. When they do not fit, segments are dropped from
/// the one before the last (the last is the close hint) until they do.
pub(super) fn key_footer(theme: &Theme, hints: &[(String, String)], width: usize) -> Line<'static> {
    let mut keep: Vec<(&str, &str)> = hints
        .iter()
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, label)| (key.as_str(), label.as_str()))
        .collect();
    let cells = |keep: &[(&str, &str)]| {
        keep.iter()
            .map(|(key, label)| text::width(key) + 1 + text::width(label))
            .sum::<usize>()
            + 3 * keep.len().saturating_sub(1)
    };
    while keep.len() > 1 && cells(&keep) + 1 > width {
        keep.remove(keep.len() - 2);
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(chrome::key_hint_spans(theme, &keep));
    Line::from(fit(spans, width))
}

#[cfg(test)]
mod tests;

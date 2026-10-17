//! The model selector: a left column (Roles, All models, Favorites, Get models, providers with counts and
//! a state glyph) and a right pane that is either the Roles view, a searchable list of models with a kind
//! filter, a detail line and the roles that use the highlighted model, or the Get models group (installed
//! local models, Hugging Face search, GGUF quantizations and curated embedding models; see
//! `model_download`). It only reports what the user chose; the controller saves it and answers with
//! [`ModelOverlay::set_roles`].
use super::model_download::{ModelDownloads, Step};
use super::{
    DrawCtx, OverlayEvent,
    settings::{
        filled, fit, highlighted, hint, hint_line, is_ctrl, match_style, other_scope, pair_label,
        panel, pressed, scope_name, scroll_cell, typed_char,
    },
};
use crate::{
    config::paths::Layout,
    config::settings_store::Scope,
    llm::catalog::{Discovery, ModelEntry, ModelKind, ProviderStatus},
    llm::model_config::{ModelSelection, ROLES, RoleSource, RoleStatus},
    setup::embeddings,
    stats::local_models::Settings,
    ui::{
        chrome::{self, Section},
        keys::{Action, Keymap},
        status::Lora,
        text,
        theme::{Sym, Theme, Tok},
    },
};
use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KindFilter {
    All,
    Chat,
    Embedding,
    Other,
}

impl KindFilter {
    const ALL: [KindFilter; 4] = [
        KindFilter::All,
        KindFilter::Chat,
        KindFilter::Embedding,
        KindFilter::Other,
    ];
    fn label(self) -> &'static str {
        match self {
            KindFilter::All => "all",
            KindFilter::Chat => "chat",
            KindFilter::Embedding => "embedding",
            KindFilter::Other => "other",
        }
    }
    fn accepts(self, entry: &ModelEntry) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Chat => entry.supports_chat(),
            KindFilter::Embedding => entry.is_embedding(),
            KindFilter::Other => !entry.supports_chat() && !entry.is_embedding(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Group {
    Roles,
    All,
    Favorites,
    Download,
    Provider(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Groups,
    List,
}

struct Chooser {
    selection: ModelSelection,
    name: String,
    /// Roles this model can be assigned to (see `role_accepts`).
    roles: Vec<&'static str>,
    cursor: usize,
}

/// Typed-id prompt for the roles that take a checkpoint path or Hugging Face repo id.
struct Prompt {
    role: &'static str,
    text: String,
}

/// Saved declarations and their resolved HTTP route are separate so inheritance
/// remains live when a favorite is assigned or persisted again.
pub struct FavoriteRoute {
    pub selection: ModelSelection,
    pub effective: ModelSelection,
}

struct FavoriteEntry {
    model: ModelEntry,
    summary: String,
}

pub struct ModelOverlay {
    discovery: Option<Discovery>,
    roles: Vec<RoleStatus>,
    /// Memory backend needs the memory role (see `with_memory_required`).
    memory_required: bool,
    /// Memory backend uses the memory role only for optional automatic saving (semantic auto-retain).
    memory_optional: bool,
    favorites: Vec<FavoriteRoute>,
    favorite_entries: Vec<FavoriteEntry>,
    scope: Scope,
    group: Group,
    focus: Focus,
    kind: KindFilter,
    search: String,
    cursor: usize,
    scroll: usize,
    page: usize,
    /// Picking a model for this role (started from the Roles view).
    target_role: Option<String>,
    chooser: Option<Chooser>,
    prompt: Option<Prompt>,
    headers_editor: Option<(String, ModelSelection, super::headers::HeaderEditor)>,
    /// The groups column is hidden at this width (set while drawing), so it cannot take focus.
    narrow: bool,
    /// Banner near the footer: (text, is_ok). Cleared by the next key press.
    message: Option<(String, bool)>,
    /// LoRA state of the current main model, which the catalog alone cannot know (admitted adapters).
    main_lora: Lora,
    learning: bool,
    /// The judge role's Laya entry, offered only while choosing the judge model (see `laya_judge_entry`).
    laya_judge: ModelEntry,
    /// The Get models group: installed local models, downloads and the embedding models.
    downloads: ModelDownloads,
}

fn is_embedding(entry: &ModelEntry) -> bool {
    entry.is_embedding()
}

/// Whether a role can take this entry. Embedding models are offered only to the `embedding` role (they
/// cannot chat); `decision` takes a typed Laya checkpoint, never a catalog entry; the Laya engine entry
/// answers only the `judge` role's typed choices.
fn role_accepts(role: &str, entry: &ModelEntry) -> bool {
    if entry.selection.provider == crate::config::laya::PROVIDER {
        return role == "judge";
    }
    match role {
        "decision" => false,
        "embedding" => is_embedding(entry),
        _ => entry.supports_chat(),
    }
}

/// [`role_accepts`] for the list picked from a role row. The embedding role picker also offers
/// models whose server did not say what they are (unknown kind), labelled unclassified, so a pulled
/// embedding model on a server that declares no type stays assignable. Classified chat models stay
/// excluded, and the per-model role chooser keeps the strict rule.
fn role_picker_accepts(role: &str, entry: &ModelEntry) -> bool {
    role_accepts(role, entry)
        || (role == "embedding"
            && entry.selection.provider != crate::config::laya::PROVIDER
            && is_unclassified(entry))
}

fn is_unclassified(entry: &ModelEntry) -> bool {
    entry.metadata.kind == ModelKind::Unknown
}

/// Label for an unclassified model in the embedding role picker.
pub const UNCLASSIFIED_LABEL: &str = "unclassified — may not be an embedding model";

/// Models the embedding role can take, classified embedding models first, then unclassified ones
/// (`true` marks a classified embedding model). Unavailable models are left out.
pub fn embedding_candidates(discovery: &Discovery) -> Vec<(ModelEntry, bool)> {
    let mut out: Vec<(ModelEntry, bool)> = discovery
        .models
        .iter()
        .filter(|entry| entry.available && role_picker_accepts("embedding", entry))
        .map(|entry| (entry.clone(), is_embedding(entry)))
        .collect();
    out.sort_by_key(|(_, classified)| !*classified);
    out
}

/// The Laya decision engine as a judge model. It shares the decision role's checkpoint (assigned or
/// installed), so it is selectable only once the decision role has one.
fn laya_judge_entry(roles: &[RoleStatus]) -> ModelEntry {
    let checkpoint = roles
        .iter()
        .find(|status| status.role == "decision")
        .and_then(|status| status.effective.clone());
    ModelEntry {
        selection: ModelSelection::new(
            crate::config::laya::PROVIDER,
            crate::config::laya::JUDGE_MODEL,
        ),
        label: "decision checkpoint".into(),
        source: match &checkpoint {
            Some(checkpoint) => format!("Laya typed choices · decision role: {checkpoint}"),
            None => "Laya typed choices · no decision checkpoint".into(),
        },
        available: checkpoint.is_some(),
        metadata: Default::default(),
    }
}

fn source_label(source: RoleSource) -> &'static str {
    match source {
        RoleSource::Assigned => "assigned",
        RoleSource::Main => "session model",
        RoleSource::FollowsMain => "follows main",
        RoleSource::FollowsSmall => "follows small",
        RoleSource::LayaInstalled => "installed Laya checkpoint",
        RoleSource::NotSet => "not set",
    }
}

fn same_model(a: &ModelSelection, b: &ModelSelection) -> bool {
    a.provider == b.provider
        && a.model == b.model
        && a.base_url == b.base_url
        && a.protocol
            .unwrap_or_else(|| crate::llm::model_config::ApiProtocol::builtin(&a.provider))
            == b.protocol
                .unwrap_or_else(|| crate::llm::model_config::ApiProtocol::builtin(&b.provider))
        && a.account_id == b.account_id
        && a.headers == b.headers
        && a.api_key_env == b.api_key_env
}

fn display_name(entry: &ModelEntry) -> String {
    text::one_line(&match &entry.selection.account_id {
        Some(id) => format!(
            "{}/{} [account {id}]",
            entry.selection.provider, entry.label
        ),
        None => format!("{}/{}", entry.selection.provider, entry.label),
    })
}

fn capabilities(entry: &ModelEntry) -> Vec<(Sym, &'static str, Option<bool>)> {
    if is_embedding(entry) {
        return vec![(Sym::IconEmbedding, "embedding", Some(true))];
    }
    vec![
        (Sym::IconVision, "vision", entry.metadata.vision),
        (Sym::IconTools, "tools", entry.metadata.tools),
        (Sym::IconReasoning, "reasoning", entry.metadata.reasoning),
    ]
}

fn capability_badges(theme: &Theme, entry: &ModelEntry) -> String {
    if !entry.supports_chat() && !entry.is_embedding() {
        return entry.metadata.kind.label().to_owned();
    }
    let capabilities = capabilities(entry);
    let mut badges = capabilities
        .iter()
        .filter(|(_, _, supported)| *supported == Some(true))
        .map(|(icon, _, _)| theme.sym(*icon).to_owned())
        .collect::<Vec<_>>();
    if capabilities
        .iter()
        .any(|(_, _, supported)| supported.is_none())
    {
        badges.push("?".into());
    }
    if badges.is_empty() {
        "-".into()
    } else {
        badges.join(" ")
    }
}

fn capability_detail(theme: &Theme, entry: &ModelEntry) -> String {
    if !entry.supports_chat() && !entry.is_embedding() {
        return format!("{} · no compatible Dial role", entry.metadata.kind.label());
    }
    let details = capabilities(entry)
        .iter()
        .map(|(icon, name, supported)| {
            format!(
                "{} {name}{}",
                theme.sym(*icon),
                match supported {
                    Some(true) => "",
                    Some(false) => " no",
                    None => " unknown",
                }
            )
        })
        .collect::<Vec<_>>()
        .join("  ");
    if entry.metadata.kind == ModelKind::Unknown {
        format!("kind unknown  {details}")
    } else {
        details
    }
}

fn capability_cell(theme: &Theme, entry: &ModelEntry, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let badges = text::ellipsize(
        &capability_badges(theme, entry),
        width.saturating_sub(1),
        theme.ascii(),
    );
    format!(
        "{}{} ",
        " ".repeat(width.saturating_sub(text::width(&badges) + 1)),
        badges
    )
}

/// `loaded`, `loads on first use`, `embedding`, `unavailable` or `available`, from what the catalog reported.
fn state_of(entry: &ModelEntry) -> &'static str {
    let source = entry.source.to_lowercase();
    if !entry.available {
        "unavailable"
    } else if !entry.supports_chat() {
        entry.metadata.kind.label()
    } else if source.contains("loads on first use") {
        "loads on first use"
    } else if source.contains("loaded") {
        "loaded"
    } else {
        "available"
    }
}

/// Why a model cannot be selected.
fn refusal(entry: &ModelEntry) -> String {
    if entry.selection.provider == crate::config::laya::PROVIDER {
        return "no Laya checkpoint; run `dial laya install --checkpoint PATH` or assign the decision role"
            .to_owned();
    }
    let source = entry.source.to_lowercase();
    if entry.is_embedding() && is_local_mlx(entry) {
        "local embedding inference is not managed by Dial; serve it through an embeddings endpoint and assign the embedding role".to_owned()
    } else if source.contains("cached, unverified") {
        "its provider is not reachable; only cached metadata is shown".to_owned()
    } else if source.contains("unverified") {
        "configured but not verified on this machine".to_owned()
    } else {
        "the provider reports it as unavailable".to_owned()
    }
}

fn is_local_mlx(entry: &ModelEntry) -> bool {
    entry.selection.provider == "mlx" && entry.selection.base_url.is_none()
}

/// The LoRA column: whether dial can train this model, and for the main model whether an adapter is on.
fn lora_cell(entry: &ModelEntry, main: Option<Lora>) -> (String, Tok) {
    if !entry.metadata.lora_verified
        && let Some(check) = entry.metadata.lora_check
    {
        return (
            match check {
                crate::llm::catalog::LoraCheck::Waiting => "waiting compute",
                crate::llm::catalog::LoraCheck::Checking => "verifying",
                crate::llm::catalog::LoraCheck::Failed => "check failed",
            }
            .into(),
            Tok::Muted,
        );
    }
    match main {
        Some(Lora::Unsupported) if is_local_mlx(entry) => return ("unsupported".into(), Tok::Dim),
        Some(Lora::Unverified) => return ("unverified".into(), Tok::Muted),
        Some(Lora::Active(id)) => return (format!("accepted v{id}"), Tok::Muted),
        Some(Lora::Ready) => return ("awaiting training".into(), Tok::Muted),
        Some(Lora::Training) => return ("training".into(), Tok::StatusLearning),
        Some(Lora::Paused) => return ("paused".into(), Tok::StatusLearning),
        Some(Lora::Evaluating) => return ("evaluating".into(), Tok::StatusLearning),
        Some(Lora::Loading) => return ("loading".into(), Tok::StatusLearning),
        Some(Lora::Loaded) => return ("loaded".into(), Tok::Success),
        Some(Lora::Used) => return ("used last response".into(), Tok::Success),
        Some(Lora::Failed) => return ("request failed".into(), Tok::Muted),
        Some(Lora::Off) => return ("LoRA off".into(), Tok::Muted),
        _ => {}
    }
    if !is_local_mlx(entry) {
        return ("-".into(), Tok::Dim);
    }
    if entry.metadata.lora_verified {
        return (
            if entry.metadata.lora == Some(true) {
                "verified"
            } else {
                "unsupported"
            }
            .into(),
            Tok::Muted,
        );
    }
    match entry.metadata.lora {
        Some(true) => ("unverified".into(), Tok::Muted),
        Some(false) => ("unsupported".into(), Tok::Dim),
        None => ("LoRA ?".into(), Tok::Muted),
    }
}

/// One sentence for the detail line on LoRA weight training.
fn lora_detail(entry: &ModelEntry, main: Option<Lora>) -> String {
    if !entry.metadata.lora_verified
        && let Some(check) = entry.metadata.lora_check
    {
        return match check {
                crate::llm::catalog::LoraCheck::Waiting => "LoRA verification queued; waiting for local compute.",
                crate::llm::catalog::LoraCheck::Checking => "Checking adapter targets, a tiny gradient update, and adapter save/reload. This verification does not create a production adapter.",
                crate::llm::catalog::LoraCheck::Failed => "Verification could not complete. F6 retries and shows diagnostics; compatibility is still unverified.",
            }.into();
    }
    match main {
        Some(Lora::Unsupported) if is_local_mlx(entry) => return "LoRA unavailable in Dial's current trainer. /improve shows the model capability probe's reason.".into(),
        Some(Lora::Unverified) => return "MLX architecture found; LoRA training targets have not been verified by Dial.".into(),
        Some(Lora::Active(id)) => return format!("LoRA v{id} accepted; loading not confirmed in this session. /improve shows evaluation results."),
        Some(Lora::Ready) => return "LoRA compatible; awaiting training and acceptance. No adapter use confirmed. See /improve.".into(),
        Some(Lora::Training) => return "LoRA candidate is training; it must pass evaluation before activation.".into(),
        Some(Lora::Paused) => return "LoRA training paused for coding with a saved checkpoint; it resumes when compute is idle. The admitted adapter stays active.".into(),
        Some(Lora::Evaluating) => return "LoRA candidate is being evaluated against the baseline; not yet accepted.".into(),
        Some(Lora::Loading) => return "Loading the adapter into the local server; generation not yet confirmed.".into(),
        Some(Lora::Loaded) => return "Adapter loaded and startup generation check passed; awaiting a successful response.".into(),
        Some(Lora::Used) => return "Adapter used on the last successful model response in this session. /improve shows its path and gate scores.".into(),
        Some(Lora::Failed) => return "Adapter loading or the latest request failed; successful use is not confirmed for that request.".into(),
        Some(Lora::Off) => return "LoRA compatible, but training or automatic improvement is disabled.".into(),
        _ => {}
    }
    if !is_local_mlx(entry) {
        return format!(
            "no LoRA: served by {}; pick an mlx/ model to train adapters",
            entry.selection.provider
        );
    }
    if entry.metadata.lora_verified {
        return if entry.metadata.lora == Some(true) { "Verified: adapter gradient update and save/reload passed. This does not measure training quality." }
            else { "Verified unsupported by Dial's trainer. Press F6 to recheck and display the reason." }.into();
    }
    match entry.metadata.lora {
        Some(true) => {
            "MLX architecture found; LoRA training targets have not been verified by Dial.".into()
        }
        Some(false) => "no LoRA: the installed mlx-lm does not implement this architecture".into(),
        None => "LoRA unknown: install the MLX environment with `dial setup` to check".into(),
    }
}

fn fmt_context(tokens: u32) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1024 && tokens.is_multiple_of(1024) {
        format!("{}k", tokens / 1024)
    } else if tokens >= 1000 {
        format!("{}k", (tokens + 500) / 1000)
    } else {
        tokens.to_string()
    }
}

fn provider_glyph(theme: &Theme, state: &str) -> (String, Tok) {
    match state {
        "reachable" | "cached" => (theme.sym(Sym::RadioOn).to_owned(), Tok::Success),
        "auth-required" => (theme.sym(Sym::Warn).to_owned(), Tok::Warning),
        "disabled" => (theme.sym(Sym::RadioOff).to_owned(), Tok::Dim),
        _ => (theme.sym(Sym::Cross2).to_owned(), Tok::Error),
    }
}

impl ModelOverlay {
    pub fn new(discovery: Option<Discovery>, roles: Vec<RoleStatus>, scope: Scope) -> Self {
        ModelOverlay {
            memory_required: false,
            memory_optional: false,
            laya_judge: laya_judge_entry(&roles),
            discovery,
            roles,
            favorites: Vec::new(),
            favorite_entries: Vec::new(),
            scope,
            group: Group::All,
            focus: Focus::List,
            kind: KindFilter::All,
            search: String::new(),
            cursor: 0,
            scroll: 0,
            page: 8,
            target_role: None,
            chooser: None,
            prompt: None,
            headers_editor: None,
            narrow: false,
            message: None,
            main_lora: Lora::Unknown,
            learning: false,
            downloads: ModelDownloads::idle(),
        }
        .with_discovered_embeddings()
    }

    /// Hands the discovered embedding models to the Get models Embedding shelf.
    fn with_discovered_embeddings(mut self) -> Self {
        if let Some(discovery) = &self.discovery {
            self.downloads.set_embedding_models(&discovery.models);
        }
        self
    }

    pub fn with_favorites(mut self, favorites: Vec<FavoriteRoute>) -> Self {
        self.set_favorites(favorites);
        self
    }

    /// Apply persisted favorites only after the controller reports the saved configuration.
    pub fn set_favorites(&mut self, favorites: Vec<FavoriteRoute>) {
        let anchor = self.highlighted().map(|entry| entry.selection.clone());
        self.favorites = favorites;
        self.rebuild_favorite_entries();
        if self.group != Group::Roles {
            let rows = self.rows();
            self.cursor = anchor
                .and_then(|selection| {
                    rows.iter()
                        .position(|(entry, _)| same_model(&entry.selection, &selection))
                })
                .unwrap_or_else(|| self.cursor.min(rows.len().saturating_sub(1)));
            self.scroll = self.scroll.min(self.cursor);
        }
    }

    fn favorite_matches(route: &FavoriteRoute, selection: &ModelSelection) -> bool {
        same_model(&route.selection, selection) || same_model(&route.effective, selection)
    }

    fn is_favorite(&self, selection: &ModelSelection) -> bool {
        self.favorites
            .iter()
            .any(|route| Self::favorite_matches(route, selection))
    }

    fn rebuild_favorite_entries(&mut self) {
        self.favorite_entries.clear();
        let Some(discovery) = &self.discovery else {
            return;
        };
        for route in &self.favorites {
            let selection = &route.effective;
            let Some(template) = discovery
                .models
                .iter()
                .filter(|entry| {
                    let known = &entry.selection;
                    known.provider == selection.provider
                        && known.model == selection.model
                        && known
                            .base_url
                            .as_deref()
                            .map(|url| url.trim_end_matches('/'))
                            == selection
                                .base_url
                                .as_deref()
                                .map(|url| url.trim_end_matches('/'))
                        && known.protocol.unwrap_or_else(|| {
                            crate::llm::model_config::ApiProtocol::builtin(&known.provider)
                        }) == selection.protocol.unwrap_or_else(|| {
                            crate::llm::model_config::ApiProtocol::builtin(&selection.provider)
                        })
                })
                .max_by_key(|entry| entry.available)
            else {
                continue;
            };
            let mut entry = template.clone();
            entry.selection = route.selection.clone();
            let mut policy = Vec::new();
            if let Some(headers) = &selection.headers {
                for header in headers {
                    policy.push(match &header.value {
                        crate::llm::model_config::HeaderValue::Env(name) => {
                            format!("{}=env({name})", header.name)
                        }
                        crate::llm::model_config::HeaderValue::Literal(value) => {
                            format!("{}=literal({value})", header.name)
                        }
                    });
                }
            }
            if let Some(name) = &selection.api_key_env {
                policy.push(format!("key env({name})"));
            }
            if route.selection.headers.as_ref().is_some_and(Vec::is_empty) {
                policy.push("headers cleared".into());
            } else if route.selection.headers.is_none() && !policy.is_empty() {
                policy.insert(0, "headers inherited".into());
            }
            let policy = policy.join(", ");
            if !policy.is_empty() {
                entry.label.push_str(" [");
                entry.label.push_str(&policy);
                entry.label.push(']');
            }
            let protocol = selection.protocol.unwrap_or_else(|| {
                crate::llm::model_config::ApiProtocol::builtin(&selection.provider)
            });
            let mut summary = format!(
                "favorite route {} ({})",
                selection
                    .base_url
                    .as_deref()
                    .unwrap_or("no configured HTTP endpoint"),
                protocol.as_str()
            );
            if let Some(account) = &selection.account_id {
                summary.push_str(&format!("; account {account}"));
            }
            if !policy.is_empty() {
                summary.push_str(&format!("; {policy}"));
            }
            if !same_model(&template.selection, selection) {
                summary.push_str("; routing/access unverified until inference");
            }
            self.favorite_entries.push(FavoriteEntry {
                model: entry,
                summary,
            });
        }
    }

    fn all_entries(&self) -> impl Iterator<Item = &ModelEntry> {
        let judging = self.target_role.as_deref() == Some("judge");
        std::iter::once(&self.laya_judge)
            .filter(move |_| judging)
            .chain(
                self.discovery
                    .iter()
                    .flat_map(|discovery| discovery.models.iter())
                    .filter(|entry| !self.is_favorite(&entry.selection)),
            )
            .chain(self.favorite_entries.iter().map(|entry| &entry.model))
    }

    fn toggle_favorite(&mut self) -> OverlayEvent {
        if self.focus != Focus::List {
            return OverlayEvent::None;
        }
        let Some(selection) = self.highlighted().map(|entry| entry.selection.clone()) else {
            return OverlayEvent::None;
        };
        let mut favorites: Vec<ModelSelection> = self
            .favorites
            .iter()
            .filter(|route| !Self::favorite_matches(route, &selection))
            .map(|route| route.selection.clone())
            .collect();
        if !self.is_favorite(&selection) {
            favorites.push(selection);
        }
        match toml::Value::try_from(favorites) {
            Ok(value) => OverlayEvent::Set {
                path: "model_favorites".into(),
                value,
                scope: self.scope,
            },
            Err(error) => {
                self.set_status(format!("Cannot save favorites: {error}"), true);
                OverlayEvent::None
            }
        }
    }

    pub fn with_learning(mut self, enabled: bool) -> Self {
        self.learning = enabled;
        if !enabled {
            self.roles.retain(|role| role.role != "decision");
        }
        self
    }

    /// Report the main model's LoRA state (an admitted adapter, or training switched off).
    pub fn with_main_lora(mut self, lora: Lora) -> Self {
        self.main_lora = lora;
        self
    }

    /// Reads the installed local models and enables downloads in the Get models group. Without it the
    /// group lists nothing installed and downloads report unavailable.
    pub fn with_downloads(mut self, layout: &Layout, settings: Settings, python: PathBuf) -> Self {
        self.downloads = ModelDownloads::open(layout, settings, python);
        if let Some(discovery) = &self.discovery {
            self.downloads.set_embedding_models(&discovery.models);
        }
        self
    }

    /// The main model's LoRA state when `entry` is the main model.
    fn main_lora_for(&self, entry: &ModelEntry) -> Option<Lora> {
        self.role_selection("main")
            .filter(|main| same_model(main, &entry.selection))
            .map(|_| self.main_lora)
    }

    /// Open on the Roles view (used by the "assign models to roles" shortcut).
    pub fn show_roles(&mut self) {
        self.group = Group::Roles;
        self.focus = Focus::List;
        self.search.clear();
        self.kind = KindFilter::All;
        self.target_role = None;
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Open on the Get models group (`/models local`).
    pub fn show_downloads(&mut self) {
        self.group = Group::Download;
        self.focus = Focus::List;
        self.search.clear();
        self.kind = KindFilter::All;
        self.target_role = None;
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Whether a finished download asks for a rescan; the controller takes the request once per frame.
    pub fn take_refresh(&mut self) -> bool {
        self.downloads.take_rescan()
    }

    /// Discovery arrives asynchronously: the search, filters and the highlighted model survive.
    pub fn set_discovery(&mut self, d: Discovery) {
        let anchor = self.highlighted().map(|e| e.selection.clone());
        self.downloads.set_embedding_models(&d.models);
        self.discovery = Some(d);
        self.rebuild_favorite_entries();
        if let Group::Provider(id) = &self.group
            && !self.providers().any(|p| &p.id == id)
        {
            self.group = Group::All;
        }
        // Role rows are independent of discovery. A background refresh must not
        // move the user's role cursor to main.
        if self.group == Group::Roles {
            return;
        }
        let rows = self.rows();
        self.cursor = anchor
            .and_then(|a| {
                rows.iter()
                    .position(|(entry, _)| same_model(&entry.selection, &a))
            })
            .unwrap_or_else(|| self.cursor.min(rows.len().saturating_sub(1)));
    }

    /// Whether the memory backend needs the memory role; an unassigned memory role then shows as required.
    pub fn with_memory_required(mut self, required: bool) -> Self {
        self.memory_required = required;
        self
    }

    pub fn set_memory_required(&mut self, required: bool) {
        self.memory_required = required;
    }

    /// Whether the memory backend uses the memory role only for optional automatic saving.
    pub fn set_memory_optional(&mut self, optional: bool) {
        self.memory_optional = optional;
    }

    pub fn set_roles(&mut self, roles: Vec<RoleStatus>) {
        let anchor = self.highlighted().map(|e| e.selection.clone());
        self.laya_judge = laya_judge_entry(&roles);
        self.roles = roles;
        if self.group != Group::Roles {
            let rows = self.rows();
            self.cursor = anchor
                .and_then(|a| {
                    rows.iter()
                        .position(|(entry, _)| same_model(&entry.selection, &a))
                })
                .unwrap_or_else(|| self.cursor.min(rows.len().saturating_sub(1)));
        }
    }

    fn role_selection(&self, role: &str) -> Option<&ModelSelection> {
        self.roles
            .iter()
            .find(|status| status.role == role)
            .and_then(|status| status.assigned.as_ref())
    }

    fn roles_using(&self, selection: &ModelSelection) -> Vec<&str> {
        ROLES
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| {
                self.role_selection(name)
                    .is_some_and(|s| same_model(s, selection))
            })
            .collect()
    }

    fn assigned_roles(&self) -> usize {
        ROLES
            .iter()
            .filter(|(name, _)| self.role_selection(name).is_some())
            .count()
    }

    fn count_in(&self, group: &Group) -> usize {
        if self.discovery.is_none() && *group != Group::Download {
            return 0;
        }
        match group {
            Group::Roles => ROLES.len(),
            Group::All => self.all_entries().count(),
            Group::Favorites => self.favorite_entries.len(),
            Group::Download => self.downloads.installed_count(),
            Group::Provider(id) => self
                .all_entries()
                .filter(|entry| &entry.selection.provider == id)
                .count(),
        }
    }

    /// Entries of the right pane in display order, with the positions matched by the search (char
    /// indexes into the displayed name). Current main model first, then loaded, available, the rest.
    fn rows(&self) -> Vec<(&ModelEntry, Vec<usize>)> {
        if self.discovery.is_none() {
            return Vec::new();
        }
        if matches!(self.group, Group::Roles | Group::Download) {
            return Vec::new();
        }
        let main = self.role_selection("main");
        let mut rows: Vec<(u8, i32, usize, &ModelEntry, Vec<usize>)> = Vec::new();
        for (index, entry) in self.all_entries().enumerate() {
            if let Group::Provider(id) = &self.group
                && &entry.selection.provider != id
            {
                continue;
            }
            if self.group == Group::Favorites && !self.is_favorite(&entry.selection) {
                continue;
            }
            if !self.kind.accepts(entry) {
                continue;
            }
            let name = display_name(entry);
            let (score, positions) = if self.search.is_empty() {
                (0, Vec::new())
            } else {
                match text::fuzzy(&self.search, &name).or_else(|| {
                    text::fuzzy(
                        &self.search,
                        &format!(
                            "{name} {} {}",
                            text::one_line(&entry.source),
                            capabilities(entry)
                                .iter()
                                .filter(|(_, _, supported)| *supported == Some(true))
                                .map(|(_, label, _)| *label)
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                    )
                    .map(|(s, _)| (s - 50, Vec::new()))
                }) {
                    Some(hit) => hit,
                    None => continue,
                }
            };
            // The Laya engine leads the judge list: it is not a catalog model and is easy to miss.
            let rank = if main.is_some_and(|m| same_model(m, &entry.selection))
                || entry.selection.provider == crate::config::laya::PROVIDER
            {
                0
            } else if self.is_favorite(&entry.selection) {
                1
            } else if entry.available && state_of(entry) == "loaded" {
                2
            } else if entry.available {
                3
            } else {
                4
            };
            rows.push((rank, score, index, entry, positions));
        }
        // With a query the best match leads within a rank; without one the discovery order stands.
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        if let Some(role) = &self.target_role {
            rows.retain(|row| role_picker_accepts(role, row.3));
            if role == "embedding" {
                // Classified embedding models lead; unclassified ones follow (stable sort).
                rows.sort_by_key(|row| !is_embedding(row.3));
            }
        }
        rows.into_iter()
            .map(|(_, _, _, entry, positions)| (entry, positions))
            .collect()
    }

    fn highlighted(&self) -> Option<&ModelEntry> {
        if matches!(self.group, Group::Roles | Group::Download) {
            return None;
        }
        let rows = self.rows();
        let at = self.cursor.min(rows.len().saturating_sub(1));
        rows.get(at).map(|(entry, _)| *entry)
    }

    fn providers(&self) -> impl Iterator<Item = &ProviderStatus> {
        self.discovery
            .iter()
            .flat_map(|d| d.providers.iter())
            .filter(|p| p.models > 0)
    }

    fn groups(&self) -> Vec<Group> {
        let mut groups = vec![Group::Roles, Group::All, Group::Favorites, Group::Download];
        groups.extend(self.providers().map(|p| Group::Provider(p.id.clone())));
        groups
    }

    fn list_len(&self) -> usize {
        if self.group == Group::Roles {
            ROLES.len()
        } else {
            self.rows().len()
        }
    }

    // ---- input ----------------------------------------------------------------------------

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        match event {
            Event::Key(key) if pressed(key) => self.key(key, keys),
            Event::Paste(text) if self.headers_editor.is_some() => {
                if let Some((_, _, editor)) = &mut self.headers_editor {
                    editor.paste(text);
                }
                OverlayEvent::None
            }
            Event::Paste(text) if self.prompt.is_some() => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.text.push_str(&text::one_line(text));
                }
                OverlayEvent::None
            }
            Event::Paste(text) if self.chooser.is_none() => {
                self.type_text(&text::one_line(text));
                OverlayEvent::None
            }
            _ => OverlayEvent::None,
        }
    }

    fn type_text(&mut self, text: &str) {
        if self.group == Group::Download {
            self.focus = Focus::List;
            self.downloads.paste(text);
            return;
        }
        if self.group == Group::Roles {
            self.group = Group::All;
            self.target_role = None;
        }
        self.focus = Focus::List;
        self.search.push_str(text);
        self.cursor = 0;
        self.scroll = 0;
    }

    /// One-line banner for the controller's verdict on an assignment (success or error); cleared by the next key press.
    pub fn set_status(&mut self, message: String, is_error: bool) {
        self.message = Some((message, !is_error));
    }

    fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        self.message = None;
        if let Some((role, selection, editor)) = &mut self.headers_editor {
            match editor.key(key, keys) {
                super::headers::Outcome::Pending => return OverlayEvent::None,
                super::headers::Outcome::Cancel => {
                    self.headers_editor = None;
                    return OverlayEvent::None;
                }
                super::headers::Outcome::Save(headers) => {
                    selection.headers = headers;
                    let event = OverlayEvent::AssignModel {
                        role: role.clone(),
                        selection: selection.clone(),
                        scope: self.scope,
                    };
                    self.headers_editor = None;
                    return event;
                }
            }
        }
        if self.prompt.is_some() {
            return self.prompt_key(key, keys);
        }
        if self.chooser.is_some() {
            return self.chooser_key(key, keys);
        }
        if self.group == Group::Download
            && self.focus == Focus::List
            && !self.downloads.has_picker()
            && (keys.matches(Action::TabPrev, key)
                || (key.code == KeyCode::Left
                    && key.modifiers == crossterm::event::KeyModifiers::CONTROL))
        {
            if !self.narrow {
                self.focus = Focus::Groups;
            }
            return OverlayEvent::None;
        }
        if self.group == Group::Download
            && (self.focus == Focus::List
                || typed_char(key).is_some()
                || key.code == KeyCode::Backspace)
        {
            self.focus = Focus::List;
            match self.downloads.key(key, keys) {
                Step::Ignored => {}
                Step::Handled => return OverlayEvent::None,
                Step::Back => {
                    if self.narrow {
                        return OverlayEvent::Close;
                    }
                    self.focus = Focus::Groups;
                    return OverlayEvent::None;
                }
                Step::UseModel(path) => {
                    return OverlayEvent::AssignModel {
                        role: "main".into(),
                        selection: ModelSelection::new("mlx", &path),
                        scope: self.scope,
                    };
                }
                Step::SetupEmbedding(model) => {
                    return OverlayEvent::SetupDep {
                        id: embeddings::setup_id(&model),
                    };
                }
                Step::UseEmbedding(selection) => {
                    return OverlayEvent::AssignModel {
                        role: "embedding".into(),
                        selection,
                        scope: self.scope,
                    };
                }
            }
        }
        if key.code == KeyCode::F(8) {
            // Headers belong to a role's assignment; outside Roles only a role being picked for has one.
            if self.group != Group::Roles && self.target_role.is_none() {
                self.set_status(
                    "Choose a role first: Roles → Enter, then F8 sets that role's headers".into(),
                    true,
                );
                return OverlayEvent::None;
            }
            let role = if self.group == Group::Roles {
                ROLES.get(self.cursor).map(|(role, _)| (*role).to_owned())
            } else {
                self.target_role.clone()
            };
            let selection = if self.group == Group::Roles {
                role.as_deref()
                    .and_then(|role| self.role_selection(role))
                    .cloned()
            } else {
                self.highlighted().map(|entry| entry.selection.clone())
            };
            if let (Some(role), Some(selection)) = (role, selection) {
                if selection.provider == "chatgpt"
                    || (selection.provider == "mlx" && selection.base_url.is_none())
                {
                    self.set_status(
                        "This provider does not permit custom HTTP headers".into(),
                        true,
                    );
                } else {
                    let title = format!("{} → role {role}", selection.model);
                    let editor =
                        super::headers::HeaderEditor::new(&selection.headers).titled(title);
                    self.headers_editor = Some((role, selection, editor));
                }
            } else {
                self.set_status(
                    "Assign a model to this role before setting headers".into(),
                    true,
                );
            }
            return OverlayEvent::None;
        }
        if key.code == KeyCode::F(7) {
            return OverlayEvent::Action("open:model-settings".into());
        }
        if is_ctrl(key, 'f') {
            return self.toggle_favorite();
        }
        if is_ctrl(key, 's') {
            self.scope = other_scope(self.scope);
            return OverlayEvent::None;
        }
        if key.code == KeyCode::F(5) || is_ctrl(key, 'r') {
            if self.group == Group::Download {
                self.downloads.reload();
            }
            return OverlayEvent::Refresh;
        }
        if key.code == KeyCode::F(6) && self.learning {
            if self.focus != Focus::List {
                self.set_status("Highlight a model or assigned role first.".into(), true);
                return OverlayEvent::None;
            }
            let selected = if self.group == Group::Roles {
                ROLES
                    .get(self.cursor)
                    .and_then(|(role, _)| self.role_selection(role))
                    .cloned()
            } else {
                self.rows()
                    .get(self.cursor)
                    .map(|(entry, _)| entry.selection.clone())
            };
            if let Some(selection) = selected.filter(|s| {
                s.provider == "mlx"
                    && s.base_url.is_none()
                    && std::path::Path::new(&s.model).is_absolute()
            }) {
                return OverlayEvent::VerifyLora(selection);
            }
            self.set_status(
                "Select a downloaded local MLX model to verify LoRA support.".into(),
                true,
            );
            return OverlayEvent::None;
        }
        if is_ctrl(key, 'a') {
            return self.open_chooser();
        }
        if keys.matches(Action::SelectCancel, key) {
            return self.cancel();
        }
        if keys.matches(Action::SelectUp, key) {
            self.step(-1);
        } else if keys.matches(Action::SelectDown, key) {
            self.step(1);
        } else if keys.matches(Action::SelectPageUp, key) {
            self.step(-(self.page as isize));
        } else if keys.matches(Action::SelectPageDown, key) {
            self.step(self.page as isize);
        } else if keys.matches(Action::SelectConfirm, key) {
            // Nothing to pick yet: Enter goes where models come from.
            if self.focus == Focus::List && self.group != Group::Roles && self.nothing_discovered()
            {
                return OverlayEvent::Action("open:providers".into());
            }
            return self.confirm();
        } else if keys.matches(Action::SectionNext, key) || keys.matches(Action::SectionPrev, key) {
            self.focus = if self.focus == Focus::Groups || self.narrow {
                Focus::List
            } else {
                Focus::Groups
            };
        } else if keys.matches(Action::TabNext, key) {
            if self.focus == Focus::Groups {
                self.focus = Focus::List;
            } else if !matches!(self.group, Group::Roles | Group::Download) {
                self.shift_kind(1);
            }
        } else if keys.matches(Action::TabPrev, key) {
            if self.focus == Focus::List
                && !matches!(self.group, Group::Roles | Group::Download)
                && self.kind != KindFilter::All
            {
                self.shift_kind(-1);
            } else {
                if !self.narrow {
                    self.focus = Focus::Groups;
                }
            }
        } else if key.code == KeyCode::Delete
            || (self.group == Group::Roles && typed_char(key) == Some('x'))
        {
            return self.clear_role();
        } else if key.code == KeyCode::Backspace {
            if self.search.pop().is_some() {
                self.cursor = 0;
                self.scroll = 0;
            }
        } else if let Some(c) = typed_char(key) {
            self.type_text(&c.to_string());
        }
        OverlayEvent::None
    }

    fn cancel(&mut self) -> OverlayEvent {
        if let Some(role) = self.target_role.take() {
            self.show_roles();
            self.cursor = ROLES
                .iter()
                .position(|(name, _)| *name == role)
                .unwrap_or(0);
            return OverlayEvent::None;
        }
        if !self.search.is_empty() {
            self.search.clear();
            self.cursor = 0;
            self.scroll = 0;
            return OverlayEvent::None;
        }
        OverlayEvent::Close
    }

    fn step(&mut self, delta: isize) {
        if self.focus == Focus::Groups {
            let groups = self.groups();
            let at = groups.iter().position(|g| *g == self.group).unwrap_or(1) as isize;
            let next = (at + delta.signum()).clamp(0, groups.len() as isize - 1) as usize;
            if groups[next] != self.group {
                self.group = groups[next].clone();
                // Provider/favorite browsing is part of picking for the same role.
                // Only explicitly returning to Roles abandons that assignment.
                if self.group == Group::Roles {
                    self.target_role = None;
                }
                self.cursor = 0;
                self.scroll = 0;
                if self.group == Group::Roles {
                    self.search.clear();
                }
            }
            return;
        }
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let last = len - 1;
        self.cursor = match delta {
            -1 => {
                if self.cursor == 0 {
                    last
                } else {
                    self.cursor - 1
                }
            }
            1 => {
                if self.cursor >= last {
                    0
                } else {
                    self.cursor + 1
                }
            }
            d if d < 0 => self.cursor.saturating_sub(d.unsigned_abs()),
            d => (self.cursor + d as usize).min(last),
        };
    }

    fn shift_kind(&mut self, delta: isize) {
        let at = KindFilter::ALL
            .iter()
            .position(|k| *k == self.kind)
            .unwrap_or(0) as isize;
        let next = (at + delta).clamp(0, KindFilter::ALL.len() as isize - 1) as usize;
        if KindFilter::ALL[next] != self.kind {
            self.kind = KindFilter::ALL[next];
            self.cursor = 0;
            self.scroll = 0;
        }
    }

    fn confirm(&mut self) -> OverlayEvent {
        if self.focus == Focus::Groups {
            self.focus = Focus::List;
            return OverlayEvent::None;
        }
        if self.group == Group::Roles {
            let at = self.cursor.min(ROLES.len() - 1);
            let role = ROLES[at].0;
            if role == "decision" {
                self.open_prompt(role);
                return OverlayEvent::None;
            }
            self.target_role = Some(role.to_owned());
            self.group = Group::All;
            self.search.clear();
            // The embedding role filter (role_accepts) also keeps unclassified models, which the
            // Embedding kind filter would hide.
            self.kind = KindFilter::All;
            self.cursor = 0;
            self.scroll = 0;
            return OverlayEvent::None;
        }
        let Some(entry) = self.highlighted() else {
            return OverlayEvent::None;
        };
        if !entry.available {
            self.message = Some((
                format!("Cannot select {}: {}", display_name(entry), refusal(entry)),
                false,
            ));
            return OverlayEvent::None;
        }
        let target = self
            .target_role
            .as_deref()
            .unwrap_or(if entry.is_embedding() {
                "embedding"
            } else {
                "main"
            });
        if !role_picker_accepts(target, entry) {
            self.message = Some((
                format!(
                    "Cannot assign {} to {target}: {} models need a compatible task endpoint, not chat",
                    display_name(entry),
                    entry.metadata.kind.label()
                ),
                false,
            ));
            return OverlayEvent::None;
        }
        let target = target.to_owned();
        let selection = entry.selection.clone();
        let role = self.target_role.take();
        if let Some(role) = &role {
            self.show_roles();
            self.cursor = ROLES.iter().position(|(name, _)| name == role).unwrap_or(0);
        }
        OverlayEvent::AssignModel {
            role: target,
            selection,
            scope: self.scope,
        }
    }

    fn open_chooser(&mut self) -> OverlayEvent {
        let Some(entry) = self.highlighted() else {
            return OverlayEvent::None;
        };
        if !entry.available {
            self.message = Some((
                format!("Cannot select {}: {}", display_name(entry), refusal(entry)),
                false,
            ));
            return OverlayEvent::None;
        }
        let roles: Vec<_> = ROLES
            .iter()
            .map(|(role, _)| *role)
            .filter(|role| role_accepts(role, entry))
            .collect();
        if roles.is_empty() {
            self.set_status("This model cannot be assigned to a chat role.".into(), true);
            return OverlayEvent::None;
        }
        self.chooser = Some(Chooser {
            selection: entry.selection.clone(),
            name: display_name(entry),
            roles,
            cursor: 0,
        });
        OverlayEvent::None
    }

    fn chooser_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Some(chooser) = &mut self.chooser else {
            return OverlayEvent::None;
        };
        let last = chooser.roles.len() - 1;
        if keys.matches(Action::SelectCancel, key) {
            self.chooser = None;
        } else if keys.matches(Action::SelectUp, key) {
            chooser.cursor = if chooser.cursor == 0 {
                last
            } else {
                chooser.cursor - 1
            };
        } else if keys.matches(Action::SelectDown, key) {
            chooser.cursor = if chooser.cursor >= last {
                0
            } else {
                chooser.cursor + 1
            };
        } else if keys.matches(Action::SelectConfirm, key) {
            let role = chooser.roles[chooser.cursor].to_owned();
            let selection = chooser.selection.clone();
            self.chooser = None;
            return OverlayEvent::AssignModel {
                role,
                selection,
                scope: self.scope,
            };
        }
        OverlayEvent::None
    }

    fn open_prompt(&mut self, role: &'static str) {
        let text = self
            .role_selection(role)
            .map(|selection| selection.model.clone())
            .unwrap_or_default();
        self.prompt = Some(Prompt { role, text });
    }

    fn prompt_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Some(prompt) = &mut self.prompt else {
            return OverlayEvent::None;
        };
        if keys.matches(Action::SelectCancel, key) {
            self.prompt = None;
        } else if keys.matches(Action::SelectConfirm, key) {
            let role = prompt.role;
            let text = prompt.text.trim().to_owned();
            self.prompt = None;
            if text.is_empty() {
                self.message = Some((
                    "Type a checkpoint path or Hugging Face repo id first".to_owned(),
                    false,
                ));
                return OverlayEvent::None;
            }
            let provider = crate::config::laya::PROVIDER;
            return OverlayEvent::AssignModel {
                role: role.to_owned(),
                selection: ModelSelection::new(provider, &text),
                scope: self.scope,
            };
        } else if key.code == KeyCode::Backspace {
            prompt.text.pop();
        } else if let Some(c) = typed_char(key) {
            prompt.text.push(c);
        }
        OverlayEvent::None
    }

    fn clear_role(&mut self) -> OverlayEvent {
        if self.group != Group::Roles || self.focus != Focus::List {
            return OverlayEvent::None;
        }
        let (role, _) = ROLES[self.cursor.min(ROLES.len() - 1)];
        if role == "main" {
            self.message = Some((
                "The main model cannot be cleared: pick another model for it instead".to_owned(),
                false,
            ));
        } else if self.role_selection(role).is_none() {
            self.message = Some((format!("{role} already uses main"), false));
        } else {
            return OverlayEvent::ClearRole {
                role: role.to_owned(),
                scope: self.scope,
            };
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
        panel(frame, ctx.theme, Section::Dial, "Models", &extra, |w, h| {
            self.compose(w, h, ctx)
        });
    }

    fn compose(&mut self, w: u16, h: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(w), usize::from(h));
        self.downloads.poll();
        if self.group == Group::Download {
            self.downloads.load_visible();
        }
        let theme = ctx.theme;
        if let Some((_, _, editor)) = &self.headers_editor {
            return editor.lines(theme, w, h);
        }
        let body_min = h.min(3);
        let mut spare = h - body_min;
        // The Get models group draws its own header, query and status, so it takes no kind, search or
        // header rows of the list pane.
        let download_view = self.group == Group::Download;
        let mut grant = |wanted: bool| {
            if wanted && spare > 0 {
                spare -= 1;
                true
            } else {
                false
            }
        };
        let (show_footer, show_kind, show_search, show_detail, show_header, show_used) = (
            grant(true),
            grant(!download_view),
            grant(!download_view),
            grant(true),
            grant(!download_view),
            grant(true),
        );
        let flags = [
            show_footer,
            show_kind,
            show_search,
            show_detail,
            show_header,
            show_used,
        ];
        let body_rows = h - flags.iter().filter(|f| **f).count();

        let left_w = if w >= 56 { (w / 3).min(28) } else { 0 };
        // Below 56 columns the groups column is hidden; its focus must not be reachable then.
        self.narrow = left_w == 0;
        if self.narrow && self.focus == Focus::Groups {
            self.focus = Focus::List;
        }
        let right_w = w - if left_w > 0 { left_w + 1 } else { 0 };

        let roles_view = self.group == Group::Roles;
        let len = if roles_view {
            ROLES.len()
        } else {
            self.rows().len()
        };
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.page = body_rows.saturating_sub(1).max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.cursor >= self.scroll + body_rows {
            self.scroll = self.cursor + 1 - body_rows.max(1);
        }
        self.scroll = self.scroll.min(len.saturating_sub(body_rows));
        let rows = self.rows();
        let download_lines = if download_view {
            self.downloads.body(right_w, body_rows, theme)
        } else {
            Vec::new()
        };
        let overflow = len > body_rows;
        let list_w = right_w.saturating_sub(usize::from(overflow));

        // Right pane lines before the list.
        let mut right_top: Vec<Line<'static>> = Vec::new();
        if show_header {
            right_top.push(self.header_line(theme, right_w));
        }
        if show_kind && !roles_view {
            right_top.push(self.kind_line(theme, right_w));
        } else if show_kind {
            right_top.push(Line::default());
        }
        if show_search {
            right_top.push(if roles_view {
                Line::from(fit(
                    vec![Span::styled(
                        " Enter picks the model for a role".to_owned(),
                        theme.fg(Tok::Dim),
                    )],
                    right_w,
                ))
            } else {
                self.search_line(theme, right_w)
            });
        }
        let top_rows = right_top.len();
        let list_rows = body_rows;

        // The left column spans the full height of the pane area (top + list rows).
        let pane_rows = top_rows + list_rows;
        let left = if left_w > 0 {
            self.left_column(theme, left_w, pane_rows)
        } else {
            Vec::new()
        };

        let mut out: Vec<Line<'static>> = Vec::with_capacity(h);
        for y in 0..pane_rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if left_w > 0 {
                let l = left.get(y).cloned().unwrap_or_default();
                let used: usize = l.iter().map(|s| text::width(&s.content)).sum();
                spans.extend(l);
                spans.push(Span::raw(" ".repeat(left_w.saturating_sub(used))));
                spans.push(Span::styled(
                    theme.sym(Sym::BoxV).to_owned(),
                    theme.fg(Tok::BorderMuted),
                ));
            }
            if let Some(top) = right_top.get(y) {
                spans.extend(top.spans.clone());
            } else if download_view {
                if let Some(line) = download_lines.get(y) {
                    // Flattening a styled Line must retain its inherited selection style.
                    spans.extend(line.spans.iter().cloned().map(|mut span| {
                        span.style = line.style.patch(span.style);
                        span
                    }));
                }
            } else {
                let i = y - top_rows;
                let index = self.scroll + i;
                let line = if let Some(chooser) = &self.chooser {
                    self.chooser_line(theme, chooser, i, list_w)
                } else if roles_view {
                    if index < ROLES.len() {
                        self.role_line(theme, index, list_w)
                    } else {
                        filled(vec![], list_w, Style::default())
                    }
                } else if let Some((entry, positions)) = rows.get(index) {
                    self.model_line(theme, entry, positions, index == self.cursor, list_w)
                } else if rows.is_empty() {
                    let empty_rows =
                        text::wrap_words(self.empty_text().trim(), list_w.saturating_sub(2).max(1));
                    filled(
                        empty_rows
                            .get(i)
                            .map(|line| {
                                vec![Span::styled(format!(" {line}"), theme.fg(Tok::Muted))]
                            })
                            .unwrap_or_default(),
                        list_w,
                        Style::default(),
                    )
                } else {
                    filled(vec![], list_w, Style::default())
                };
                spans.extend(line.spans);
                if overflow && self.chooser.is_none() {
                    spans.push(scroll_cell(theme, i, list_rows, len, self.scroll));
                }
            }
            out.push(Line::from(fit(spans, w)));
        }

        if show_detail {
            let (first, second) = self.detail_lines(theme, rows.get(self.cursor).map(|(e, _)| *e));
            out.push(Line::from(fit(first, w)));
            if show_used {
                out.push(Line::from(fit(second, w)));
            }
        }
        if show_footer {
            out.push(hint_line(theme, &self.footer(ctx.keys), w));
        }
        out
    }

    /// The scan finished and found no models at all (not merely none matching a filter).
    fn nothing_discovered(&self) -> bool {
        self.discovery.as_ref().is_some_and(|d| d.models.is_empty())
    }

    fn empty_text(&self) -> String {
        match &self.discovery {
            None => " Scanning providers and models…".to_owned(),
            Some(d) if d.models.is_empty() => {
                " No models yet. Press Enter to set up a provider or add an API key, open Get models to download a local model, or start LM Studio or Ollama (found automatically)."
                    .to_owned()
            }
            Some(_) if self.kind == KindFilter::Embedding && self.search.is_empty() => {
                " No embedding models here. Open Get models and choose the Embedding shelf to set one up."
                    .to_owned()
            }
            Some(_) if !self.search.is_empty() => {
                format!(" No models match \"{}\"", text::one_line(&self.search))
            }
            Some(_) => " No models in this view. Try another kind or provider.".to_owned(),
        }
    }

    fn header_line(&self, theme: &Theme, w: usize) -> Line<'static> {
        let name = match &self.group {
            Group::Roles => "Roles".to_owned(),
            Group::All => "All models".to_owned(),
            Group::Favorites => "Favorites".to_owned(),
            Group::Download => "Get models".to_owned(),
            Group::Provider(id) => self
                .providers()
                .find(|p| &p.id == id)
                .map_or_else(|| id.clone(), |p| p.label.clone()),
        };
        let mut spans = vec![chrome::label(theme, &format!(" {}", text::one_line(&name)))];
        if let Some(role) = &self.target_role {
            spans.push(Span::styled(
                format!(
                    "  {} choose the model for role \"{role}\"",
                    theme.sym(Sym::Pointer)
                ),
                theme.fg(Tok::Warning),
            ));
            if role == "embedding" {
                spans.push(Span::styled(
                    format!("  · {UNCLASSIFIED_LABEL}"),
                    theme.fg(Tok::Muted),
                ));
            }
        } else if self.group != Group::Roles {
            spans.push(Span::styled(
                format!("  {}", self.rows().len()),
                theme.fg(Tok::Muted),
            ));
        }
        Line::from(fit(spans, w))
    }

    fn kind_line(&self, theme: &Theme, w: usize) -> Line<'static> {
        let mut spans = vec![chrome::label(theme, " kind ")];
        for kind in KindFilter::ALL {
            let style = chrome::tab_style(theme, kind == self.kind);
            spans.push(Span::styled(format!(" {} ", kind.label()), style));
        }
        // Legend entries are added whole while they fit, so a narrow pane never shows a clipped word.
        let legend = [
            "* favorite".to_owned(),
            format!("{} vision", theme.sym(Sym::IconVision)),
            format!("{} tools", theme.sym(Sym::IconTools)),
            format!("{} reasoning", theme.sym(Sym::IconReasoning)),
            format!("{} embedding", theme.sym(Sym::IconEmbedding)),
            "? unknown".to_owned(),
        ];
        let dim = theme.fg(Tok::Muted);
        let mut used: usize = spans.iter().map(|s| text::width(&s.content)).sum();
        for item in legend {
            let piece = format!("  {item}");
            let cells = text::width(&piece);
            if used + cells > w {
                break;
            }
            used += cells;
            spans.push(Span::styled(piece, dim));
        }
        Line::from(fit(spans, w))
    }

    fn search_line(&self, theme: &Theme, w: usize) -> Line<'static> {
        let prompt = Span::styled(
            format!(" {} ", theme.sym(Sym::Prompt)),
            theme.fg(Tok::Accent),
        );
        Line::from(fit(
            if self.search.is_empty() {
                vec![
                    prompt,
                    Span::styled("Type to search models".to_owned(), theme.fg(Tok::Dim)),
                ]
            } else {
                vec![
                    prompt,
                    Span::styled(text::one_line(&self.search), theme.fg(Tok::Text)),
                    Span::styled(
                        " ".to_owned(),
                        theme.fg(Tok::Text).add_modifier(Modifier::REVERSED),
                    ),
                ]
            },
            w,
        ))
    }

    fn left_column(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Vec<Span<'static>>> {
        let mut entries: Vec<(Option<Group>, Vec<Span<'static>>)> = Vec::new();
        let active_style = |on: bool, focus: bool| -> Style {
            if on && focus {
                chrome::selected_style(theme)
            } else if on {
                theme.fg(Tok::Accent).add_modifier(Modifier::BOLD)
            } else {
                theme.fg(Tok::Text)
            }
        };
        let line = |group: &Group,
                    label: String,
                    count: String,
                    glyph: Option<(String, Tok)>,
                    this: &Self|
         -> Vec<Span<'static>> {
            let on = *group == this.group;
            let focus = this.focus == Focus::Groups;
            let style = active_style(on, focus);
            let fill = if on && focus {
                theme.bg(Tok::SelectedBg)
            } else {
                Style::default()
            };
            let pointer = if on {
                theme.sym(Sym::Pointer).to_owned()
            } else {
                " ".to_owned()
            };
            let glyph_text = glyph
                .as_ref()
                .map(|(g, _)| format!("{g} "))
                .unwrap_or_default();
            let room = width.saturating_sub(2 + text::width(&glyph_text) + text::width(&count) + 2);
            let label = text::ellipsize(&text::one_line(&label), room, theme.ascii());
            let gap = width.saturating_sub(
                2 + text::width(&glyph_text) + text::width(&label) + text::width(&count) + 1,
            );
            let mut spans = vec![Span::styled(
                format!("{pointer} "),
                style.patch(theme.fg(Tok::Accent)),
            )];
            if let Some((g, tok)) = glyph {
                spans.push(Span::styled(format!("{g} "), theme.fg(tok).patch(fill)));
            }
            spans.push(Span::styled(label, style));
            spans.push(Span::styled(" ".repeat(gap.max(1)), fill));
            spans.push(Span::styled(
                count,
                if on && focus {
                    style
                } else {
                    theme.fg(Tok::Muted)
                },
            ));
            spans.push(Span::styled(" ".to_owned(), fill));
            spans
        };
        entries.push((
            Some(Group::Roles),
            line(
                &Group::Roles,
                "Roles".to_owned(),
                format!("{}/{}", self.assigned_roles(), ROLES.len()),
                None,
                self,
            ),
        ));
        entries.push((
            Some(Group::All),
            line(
                &Group::All,
                "All models".to_owned(),
                self.count_in(&Group::All).to_string(),
                None,
                self,
            ),
        ));
        entries.push((
            Some(Group::Favorites),
            line(
                &Group::Favorites,
                "Favorites".to_owned(),
                self.count_in(&Group::Favorites).to_string(),
                None,
                self,
            ),
        ));
        entries.push((
            Some(Group::Download),
            line(
                &Group::Download,
                "Get models".to_owned(),
                self.downloads.installed_count().to_string(),
                None,
                self,
            ),
        ));
        entries.push((
            None,
            vec![Span::styled(
                theme.sym(Sym::BoxH).repeat(width.saturating_sub(1)),
                theme.fg(Tok::BorderMuted),
            )],
        ));
        for p in self.providers() {
            let group = Group::Provider(p.id.clone());
            let glyph = provider_glyph(theme, &p.state);
            entries.push((
                Some(group.clone()),
                line(
                    &group,
                    p.label.clone(),
                    self.count_in(&group).to_string(),
                    Some(glyph),
                    self,
                ),
            ));
        }
        let at = entries
            .iter()
            .position(|(g, _)| g.as_ref() == Some(&self.group))
            .unwrap_or(0);
        let first = if at >= rows { at + 1 - rows.max(1) } else { 0 };
        entries
            .into_iter()
            .skip(first)
            .map(|(_, spans)| spans)
            .collect()
    }

    fn model_line(
        &self,
        theme: &Theme,
        entry: &ModelEntry,
        positions: &[usize],
        selected: bool,
        w: usize,
    ) -> Line<'static> {
        let fill = if selected {
            theme.bg(Tok::SelectedBg)
        } else {
            Style::default()
        };
        let dim = !entry.available;
        let name_style = if dim {
            theme.fg(Tok::Dim)
        } else if selected {
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD)
        } else {
            theme.fg(Tok::Text)
        }
        .patch(fill);
        let is_main = self
            .role_selection("main")
            .is_some_and(|m| same_model(m, &entry.selection));
        let marker = if is_main {
            theme.sym(Sym::RadioOn).to_owned()
        } else if self.is_favorite(&entry.selection) {
            "*".to_owned()
        } else {
            " ".to_owned()
        };
        let pointer = if selected {
            theme.sym(Sym::Pointer).to_owned()
        } else {
            " ".to_owned()
        };
        let context = entry
            .selection
            .context_window
            .map_or_else(|| "-".to_owned(), fmt_context);
        let state = if self.target_role.as_deref() == Some("embedding") && is_unclassified(entry) {
            "unclassified"
        } else {
            state_of(entry)
        };
        let state_w = if w >= 60 { 20 } else { 0 };
        let lora_w = if self.learning && w >= 90 {
            19
        } else if w >= 72 {
            10
        } else {
            0
        };
        let ctx_w = if w >= 40 { 7 } else { 0 };
        let caps_w = if w >= 28 { 8 } else { 0 };
        let prefix_w = text::width(&pointer) + text::width(&marker) + 2;
        let name_w = w.saturating_sub(prefix_w + caps_w + state_w + lora_w + ctx_w);
        let name = text::ellipsize(
            &display_name(entry),
            name_w.saturating_sub(1),
            theme.ascii(),
        );
        let mut spans = vec![
            Span::styled(format!("{pointer} "), theme.fg(Tok::Accent).patch(fill)),
            Span::styled(format!("{marker} "), theme.fg(Tok::Success).patch(fill)),
        ];
        spans.extend(if positions.is_empty() {
            vec![Span::styled(name.clone(), name_style)]
        } else {
            highlighted(&name, positions, name_style, match_style(theme, name_style))
        });
        spans.push(Span::styled(
            " ".repeat(name_w.saturating_sub(text::width(&name))),
            fill,
        ));
        if ctx_w > 0 {
            spans.push(Span::styled(
                text::pad(&context, ctx_w),
                theme.fg(Tok::Muted).patch(fill),
            ));
        }
        if lora_w > 0 {
            let (label, tok) = lora_cell(entry, self.main_lora_for(entry));
            spans.push(Span::styled(
                text::pad(
                    &text::ellipsize(&label, lora_w.saturating_sub(1), theme.ascii()),
                    lora_w,
                ),
                theme.fg(tok).patch(fill),
            ));
        }
        if state_w > 0 {
            let tok = match state {
                "loaded" => Tok::Success,
                "unavailable" | "embedding" => Tok::Dim,
                _ => Tok::Muted,
            };
            spans.push(Span::styled(
                text::pad(state, state_w),
                theme.fg(tok).patch(fill),
            ));
        }
        if caps_w > 0 {
            spans.push(Span::styled(
                capability_cell(theme, entry, caps_w),
                theme.fg(Tok::Muted).patch(fill),
            ));
        }
        filled(spans, w, fill)
    }

    fn role_line(&self, theme: &Theme, index: usize, w: usize) -> Line<'static> {
        let (role, description) = ROLES[index];
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
        let status = self.roles.iter().find(|status| status.role == role);
        let (value, tok) = match status
            .and_then(|status| status.effective.as_ref().map(|name| (name, status.source)))
        {
            Some((name, RoleSource::FollowsMain)) => {
                (format!("unassigned · follows main ({name})"), Tok::Dim)
            }
            Some((name, source)) => (format!("{name} · {}", source_label(source)), Tok::Accent),
            None if role == "main" => ("not configured".to_owned(), Tok::Warning),
            None if role == "memory" && self.memory_required => {
                ("required: assign a model".to_owned(), Tok::Warning)
            }
            None if role == "memory" && self.memory_optional => {
                ("optional: assign to save facts".to_owned(), Tok::Muted)
            }
            None => ("not set".to_owned(), Tok::Dim),
        };
        let entry = status
            .and_then(|status| status.assigned.as_ref())
            .and_then(|selection| {
                self.discovery
                    .as_ref()?
                    .models
                    .iter()
                    .find(|entry| same_model(selection, &entry.selection))
            });
        let caps_w = if entry.is_some() && w >= 40 { 8 } else { 0 };
        let rest = w.saturating_sub(text::width(pointer) + 1 + 12 + caps_w);
        let value_w = rest * 2 / 5;
        let desc_w = rest.saturating_sub(value_w + 2);
        let value = text::ellipsize(&value, value_w, theme.ascii());
        let desc = text::ellipsize(description, desc_w, theme.ascii());
        filled(
            vec![
                Span::styled(format!("{pointer} "), theme.fg(Tok::Accent).patch(fill)),
                Span::styled(
                    text::pad(role, 12),
                    theme
                        .fg(Tok::Text)
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        })
                        .patch(fill),
                ),
                Span::styled(value.clone(), theme.fg(tok).patch(fill)),
                Span::styled(
                    " ".repeat(value_w.saturating_sub(text::width(&value)) + 2),
                    fill,
                ),
                Span::styled(desc.clone(), theme.fg(Tok::Muted).patch(fill)),
                Span::styled(" ".repeat(desc_w.saturating_sub(text::width(&desc))), fill),
                Span::styled(
                    entry
                        .map(|entry| capability_cell(theme, entry, caps_w))
                        .unwrap_or_default(),
                    theme.fg(Tok::Muted).patch(fill),
                ),
            ],
            w,
            fill,
        )
    }

    fn chooser_line(
        &self,
        theme: &Theme,
        chooser: &Chooser,
        row: usize,
        w: usize,
    ) -> Line<'static> {
        if row == 0 {
            return filled(
                vec![chrome::label(
                    theme,
                    &format!(" Assign {} to:", text::one_line(&chooser.name)),
                )],
                w,
                Style::default(),
            );
        }
        let Some(role) = chooser.roles.get(row - 1) else {
            return filled(vec![], w, Style::default());
        };
        let description = ROLES
            .iter()
            .find(|(name, _)| name == role)
            .map_or("", |(_, description)| description);
        let selected = row - 1 == chooser.cursor;
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
        filled(
            vec![
                Span::styled(format!("   {radio} "), theme.fg(Tok::Accent).patch(fill)),
                Span::styled(text::pad(role, 12), theme.fg(Tok::Text).patch(fill)),
                Span::styled(description.to_owned(), theme.fg(Tok::Muted).patch(fill)),
            ],
            w,
            fill,
        )
    }

    fn detail_lines(
        &self,
        theme: &Theme,
        entry: Option<&ModelEntry>,
    ) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        if let Some(prompt) = &self.prompt {
            let first = vec![Span::styled(
                format!(" {}: {}_", prompt.role, prompt.text),
                theme.fg(Tok::Accent),
            )];
            let second = vec![Span::styled(
                " Enter assigns this Laya checkpoint (path or Hugging Face repo id); Esc cancels",
                theme.fg(Tok::Dim),
            )];
            return (first, second);
        }
        if let Some((message, ok)) = &self.message {
            let (glyph, tok) = if *ok {
                (Sym::Check, Tok::Success)
            } else {
                (Sym::Cross2, Tok::Error)
            };
            return (
                vec![Span::styled(
                    format!(" {} {}", theme.sym(glyph), text::one_line(message)),
                    theme.fg(tok),
                )],
                Vec::new(),
            );
        }
        if self.group == Group::Download {
            return self.downloads.detail(theme);
        }
        if self.group == Group::Roles {
            let (role, description) = ROLES[self.cursor.min(ROLES.len() - 1)];
            return (
                vec![Span::styled(
                    format!(" {role}: {description}"),
                    theme.fg(Tok::Muted),
                )],
                vec![Span::styled(
                    format!(
                        " {}",
                        if role == "main" {
                            "Enter changes the model; the main model cannot be cleared"
                        } else {
                            "Enter picks a model; x or Delete makes it follow the main model again"
                        }
                    ),
                    theme.fg(Tok::Dim),
                )],
            );
        }
        let Some(entry) = entry else {
            return (Vec::new(), Vec::new());
        };
        let mut first = vec![Span::styled(
            format!(" {}", display_name(entry)),
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        )];
        first.push(Span::styled(
            format!("  {}", capability_detail(theme, entry)),
            theme.fg(Tok::Muted),
        ));
        let context = entry.selection.context_window.map_or_else(
            || "context unknown".to_owned(),
            |c| format!("context {}", fmt_context(c)),
        );
        first.push(Span::styled(
            format!(
                "  {} {context}  {} {}",
                theme.sym(Sym::Dot),
                theme.sym(Sym::Dot),
                state_of(entry)
            ),
            theme.fg(Tok::Muted),
        ));
        if !entry.available {
            first.push(Span::styled(
                format!("  {}", refusal(entry)),
                theme.fg(Tok::Warning),
            ));
        }
        let used = self.roles_using(&entry.selection);
        let used_text = if used.is_empty() {
            "not used by any role".to_owned()
        } else {
            format!("used by: {}", used.join(", "))
        };
        let mut second = vec![Span::styled(format!(" {used_text}"), theme.fg(Tok::Muted))];
        if let Some(favorite) = self
            .favorite_entries
            .iter()
            .find(|favorite| std::ptr::eq(&favorite.model, entry))
        {
            second.push(Span::styled(
                format!(
                    "  {} {}",
                    theme.sym(Sym::Dot),
                    text::one_line(&favorite.summary)
                ),
                theme.fg(Tok::Muted),
            ));
        }
        if self.learning {
            second.push(Span::styled(
                format!(
                    "  {} {}",
                    theme.sym(Sym::Dot),
                    lora_detail(entry, self.main_lora_for(entry))
                ),
                theme.fg(Tok::Muted),
            ));
        }
        second.extend([Span::styled(
            format!(
                "  {} {}",
                theme.sym(Sym::Dot),
                text::one_line(&entry.source)
            ),
            theme.fg(Tok::Muted),
        )]);
        (first, second)
    }

    fn footer(&self, keys: &Keymap) -> Vec<String> {
        if self.prompt.is_some() {
            return vec![
                hint(keys, Action::SelectConfirm, "assign"),
                hint(keys, Action::SelectCancel, "cancel"),
            ];
        }
        if self.chooser.is_some() {
            return vec![
                format!(
                    "{} choose role",
                    pair_label(keys, Action::SelectUp, Action::SelectDown)
                ),
                hint(keys, Action::SelectConfirm, "assign"),
                hint(keys, Action::SelectCancel, "cancel"),
            ];
        }
        if self.focus == Focus::Groups {
            return vec![
                format!(
                    "{} groups",
                    pair_label(keys, Action::SelectUp, Action::SelectDown)
                ),
                hint(keys, Action::SelectConfirm, "focus list"),
                hint(keys, Action::TabNext, "focus list"),
                hint(
                    keys,
                    Action::SelectCancel,
                    if self.group == Group::Download {
                        "close"
                    } else {
                        "back"
                    },
                ),
            ];
        }
        if self.group == Group::Roles {
            return vec![
                hint(keys, Action::SelectConfirm, "pick model"),
                format!(
                    "{} roles",
                    pair_label(keys, Action::SelectUp, Action::SelectDown)
                ),
                "x clear".to_owned(),
                if self.learning {
                    "F6 verify LoRA".to_owned()
                } else {
                    String::new()
                },
                "F7 main model settings".to_owned(),
                "F8 HTTP headers".to_owned(),
                self.groups_hint(keys),
                hint(keys, Action::SelectCancel, "close"),
            ];
        }
        if self.group == Group::Download {
            let mut hints = self.downloads.footer(keys);
            if !self.narrow && !self.downloads.has_picker() {
                hints.push(self.groups_hint(keys));
            }
            hints.push("F5 rescan".to_owned());
            return hints;
        }
        if self.nothing_discovered() {
            return vec![
                hint(keys, Action::SelectConfirm, "set up a provider"),
                hint(keys, Action::SelectCancel, "close"),
            ];
        }
        vec![
            "Ctrl+F favorite".to_owned(),
            hint(
                keys,
                Action::SelectConfirm,
                if self.target_role.is_some() {
                    "assign"
                } else if self.highlighted().is_some_and(ModelEntry::is_embedding) {
                    "set embedding"
                } else {
                    "set main"
                },
            ),
            format!(
                "{} models",
                pair_label(keys, Action::SelectUp, Action::SelectDown)
            ),
            self.groups_hint(keys),
            "type to search".to_owned(),
            format!(
                "{} kind",
                pair_label(keys, Action::TabPrev, Action::TabNext)
            ),
            "Ctrl+A role".to_owned(),
            "Ctrl+S scope".to_owned(),
            "F5 rescan".to_owned(),
            if self.learning {
                "F6 verify LoRA".to_owned()
            } else {
                String::new()
            },
            if self.target_role.is_some() {
                "F8 HTTP headers".to_owned()
            } else {
                String::new()
            },
            hint(
                keys,
                Action::SelectCancel,
                if self.target_role.is_some() {
                    "back"
                } else if self.search.is_empty() {
                    "close"
                } else {
                    "clear search"
                },
            ),
        ]
    }

    /// Footer hint for moving focus to the groups column; empty while that column is hidden.
    fn groups_hint(&self, keys: &Keymap) -> String {
        if self.narrow {
            String::new()
        } else {
            hint(keys, Action::SectionPrev, "groups")
        }
    }
}

#[cfg(test)]
mod tests;

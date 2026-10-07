//! The model selector: a left column (Roles, All models, providers with counts and a state glyph)
//! and a right pane that is either the Roles view or a searchable list of models with a kind filter,
//! a detail line and the roles that use the highlighted model. It only reports what the user chose;
//! the controller saves it and answers with [`ModelOverlay::set_roles`].
use super::{
    DrawCtx, OverlayEvent,
    settings::{
        filled, fit, highlighted, hint, hint_line, is_ctrl, match_style, other_scope, pair_label,
        panel, pressed, scope_name, scroll_cell, title_line, typed_char,
    },
};
use crate::{
    llm::catalog::{Discovery, ModelEntry, ProviderStatus},
    llm::model_config::{ModelSelection, ROLES},
    storage::settings_store::Scope,
    ui::{
        keys::{Action, Keymap},
        status::Lora,
        text,
        theme::{Sym, Theme, Tok},
    },
};
use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::{
    Frame,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KindFilter {
    All,
    Chat,
    Embedding,
}

impl KindFilter {
    const ALL: [KindFilter; 3] = [KindFilter::All, KindFilter::Chat, KindFilter::Embedding];
    fn label(self) -> &'static str {
        match self {
            KindFilter::All => "all",
            KindFilter::Chat => "chat",
            KindFilter::Embedding => "embedding",
        }
    }
    fn accepts(self, entry: &ModelEntry) -> bool {
        match self {
            KindFilter::All => true,
            KindFilter::Chat => !is_embedding(entry),
            KindFilter::Embedding => is_embedding(entry),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Group {
    Roles,
    All,
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
    cursor: usize,
}

pub struct ModelOverlay {
    discovery: Option<Discovery>,
    roles: Vec<(String, Option<ModelSelection>)>,
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
    /// Banner near the footer: (text, is_ok). Cleared by the next key press.
    message: Option<(String, bool)>,
    /// LoRA state of the current main model, which the catalog alone cannot know (admitted adapters).
    main_lora: Lora,
}

fn is_embedding(entry: &ModelEntry) -> bool {
    entry.source.to_lowercase().contains("embedding")
}

fn same_model(a: &ModelSelection, b: &ModelSelection) -> bool {
    a.provider == b.provider && a.model == b.model && a.base_url == b.base_url
}

fn display_name(entry: &ModelEntry) -> String {
    text::one_line(&format!("{}/{}", entry.selection.provider, entry.label))
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
    capabilities(entry)
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
        .join("  ")
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
    if source.contains("embedding") {
        "embedding"
    } else if !entry.available {
        "unavailable"
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
    let source = entry.source.to_lowercase();
    if source.contains("embedding") {
        "embedding model, not for chat".to_owned()
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
        Some(Lora::VerificationQueued) => return ("waiting compute".into(), Tok::Muted),
        Some(Lora::Verifying) => return ("verifying".into(), Tok::Muted),
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
        Some(Lora::VerificationQueued) => return "LoRA verification is waiting for local compute.".into(),
        Some(Lora::Verifying) => return "Checking LoRA targets with a tiny gradient update and adapter save/reload.".into(),
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
    pub fn new(
        discovery: Option<Discovery>,
        roles: Vec<(String, Option<ModelSelection>)>,
        scope: Scope,
    ) -> Self {
        ModelOverlay {
            discovery,
            roles,
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
            message: None,
            main_lora: Lora::Unknown,
        }
    }

    /// Report the main model's LoRA state (an admitted adapter, or training switched off).
    pub fn with_main_lora(mut self, lora: Lora) -> Self {
        self.main_lora = lora;
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
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Discovery arrives asynchronously: the search, filters and the highlighted model survive.
    pub fn set_discovery(&mut self, d: Discovery) {
        let anchor = self.highlighted().map(|e| e.selection.clone());
        self.discovery = Some(d);
        if let Group::Provider(id) = &self.group
            && !self.providers().any(|p| &p.id == id)
        {
            self.group = Group::All;
        }
        let rows = self.rows();
        self.cursor = anchor
            .and_then(|a| {
                rows.iter()
                    .position(|(entry, _)| same_model(&entry.selection, &a))
            })
            .unwrap_or_else(|| self.cursor.min(rows.len().saturating_sub(1)));
    }

    pub fn set_roles(&mut self, roles: Vec<(String, Option<ModelSelection>)>) {
        let anchor = self.highlighted().map(|e| e.selection.clone());
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
            .find(|(name, _)| name == role)
            .and_then(|(_, selection)| selection.as_ref())
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
        let Some(d) = &self.discovery else { return 0 };
        match group {
            Group::Roles => ROLES.len(),
            Group::All => d.models.len(),
            Group::Provider(id) => d
                .models
                .iter()
                .filter(|m| &m.selection.provider == id)
                .count(),
        }
    }

    /// Entries of the right pane in display order, with the positions matched by the search (char
    /// indexes into the displayed name). Current main model first, then loaded, available, the rest.
    fn rows(&self) -> Vec<(&ModelEntry, Vec<usize>)> {
        let Some(d) = &self.discovery else {
            return Vec::new();
        };
        if self.group == Group::Roles {
            return Vec::new();
        }
        let main = self.role_selection("main");
        let mut rows: Vec<(u8, i32, usize, &ModelEntry, Vec<usize>)> = Vec::new();
        for (index, entry) in d.models.iter().enumerate() {
            if let Group::Provider(id) = &self.group
                && &entry.selection.provider != id
            {
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
            let rank = if main.is_some_and(|m| same_model(m, &entry.selection)) {
                0
            } else if entry.available && state_of(entry) == "loaded" {
                1
            } else if entry.available {
                2
            } else {
                3
            };
            rows.push((rank, score, index, entry, positions));
        }
        // With a query the best match leads within a rank; without one the discovery order stands.
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        rows.into_iter()
            .map(|(_, _, _, entry, positions)| (entry, positions))
            .collect()
    }

    fn highlighted(&self) -> Option<&ModelEntry> {
        if self.group == Group::Roles {
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
        let mut groups = vec![Group::Roles, Group::All];
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
            Event::Paste(text) if self.chooser.is_none() => {
                self.type_text(&text::one_line(text));
                OverlayEvent::None
            }
            _ => OverlayEvent::None,
        }
    }

    fn type_text(&mut self, text: &str) {
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
        if self.chooser.is_some() {
            return self.chooser_key(key, keys);
        }
        if is_ctrl(key, 's') {
            self.scope = other_scope(self.scope);
            return OverlayEvent::None;
        }
        if key.code == KeyCode::F(5) || is_ctrl(key, 'r') {
            return OverlayEvent::Refresh;
        }
        if key.code == KeyCode::F(6) {
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
            return self.confirm();
        } else if keys.matches(Action::SectionNext, key) || keys.matches(Action::SectionPrev, key) {
            self.focus = if self.focus == Focus::Groups {
                Focus::List
            } else {
                Focus::Groups
            };
        } else if keys.matches(Action::TabNext, key) {
            if self.focus == Focus::Groups {
                self.focus = Focus::List;
            } else if self.group != Group::Roles {
                self.shift_kind(1);
            }
        } else if keys.matches(Action::TabPrev, key) {
            if self.focus == Focus::List
                && self.group != Group::Roles
                && self.kind != KindFilter::All
            {
                self.shift_kind(-1);
            } else {
                self.focus = Focus::Groups;
            }
        } else if key.code == KeyCode::Delete
            || (self.group == Group::Roles
                && self.search.is_empty()
                && typed_char(key) == Some('x'))
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
        if self.target_role.take().is_some() {
            self.group = Group::Roles;
            self.cursor = 0;
            self.scroll = 0;
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
                self.target_role = None;
                self.cursor = 0;
                self.scroll = 0;
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
            self.target_role = Some(ROLES[at].0.to_owned());
            self.group = Group::All;
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
        let selection = entry.selection.clone();
        let role = self.target_role.take();
        if role.is_some() {
            self.group = Group::Roles;
            self.cursor = 0;
            self.scroll = 0;
        }
        OverlayEvent::AssignModel {
            role: role.unwrap_or_else(|| "main".to_owned()),
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
        self.chooser = Some(Chooser {
            selection: entry.selection.clone(),
            name: display_name(entry),
            cursor: 0,
        });
        OverlayEvent::None
    }

    fn chooser_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Some(chooser) = &mut self.chooser else {
            return OverlayEvent::None;
        };
        let last = ROLES.len() - 1;
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
            let role = ROLES[chooser.cursor].0.to_owned();
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
            self.message = Some((format!("{role} already follows the main model"), false));
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
        let title = title_line(
            ctx.theme,
            Sym::IconModel,
            "Models",
            &format!(
                "{} scope: {}",
                ctx.theme.sym(Sym::Dot),
                scope_name(self.scope)
            ),
        );
        panel(frame, ctx.theme, title, |w, h| self.compose(w, h, ctx));
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
        let (show_footer, show_kind, show_search, show_detail, show_header, show_used) =
            (grant(), grant(), grant(), grant(), grant(), grant());
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

    fn empty_text(&self) -> String {
        match &self.discovery {
            None => " Scanning providers and models…".to_owned(),
            Some(d) if d.models.is_empty() => {
                " No models found. Start a local server (LM Studio, Ollama) or add a provider."
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
            Group::Provider(id) => self
                .providers()
                .find(|p| &p.id == id)
                .map_or_else(|| id.clone(), |p| p.label.clone()),
        };
        let mut spans = vec![Span::styled(
            format!(" {}", text::one_line(&name)),
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
        )];
        if let Some(role) = &self.target_role {
            spans.push(Span::styled(
                format!(
                    "  {} choose the model for role \"{role}\"",
                    theme.sym(Sym::Pointer)
                ),
                theme.fg(Tok::Warning),
            ));
        } else if self.group != Group::Roles {
            spans.push(Span::styled(
                format!("  {}", self.rows().len()),
                theme.fg(Tok::Muted),
            ));
        }
        Line::from(fit(spans, w))
    }

    fn kind_line(&self, theme: &Theme, w: usize) -> Line<'static> {
        let mut spans = vec![Span::styled(" Kind ".to_owned(), theme.fg(Tok::Muted))];
        for kind in KindFilter::ALL {
            let style = if kind == self.kind {
                theme
                    .on(Tok::Accent, Tok::SelectedBg)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.fg(Tok::Muted)
            };
            spans.push(Span::styled(format!(" {} ", kind.label()), style));
        }
        // Legend entries are added whole while they fit, so a narrow pane never shows a clipped word.
        let legend = [
            format!("{} vision", theme.sym(Sym::IconVision)),
            format!("{} tools", theme.sym(Sym::IconTools)),
            format!("{} reasoning", theme.sym(Sym::IconReasoning)),
            format!("{} embedding", theme.sym(Sym::IconEmbedding)),
            "? unknown".to_owned(),
        ];
        let dim = Style::default()
            .fg(Color::Reset)
            .add_modifier(Modifier::DIM);
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
                theme
                    .on(Tok::Text, Tok::SelectedBg)
                    .add_modifier(Modifier::BOLD)
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
        let state = state_of(entry);
        let state_w = if w >= 60 { 20 } else { 0 };
        let lora_w = if w >= 90 {
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
                Style::default().fg(Color::Reset).patch(fill),
            ));
        }
        filled(spans, w, fill)
    }

    fn role_line(&self, theme: &Theme, index: usize, w: usize) -> Line<'static> {
        let (role, _) = ROLES[index];
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
        let (value, tok) = match self.role_selection(role) {
            Some(selection) => (text::one_line(&selection.display_name()), Tok::Accent),
            None if role == "main" => ("not configured".to_owned(), Tok::Warning),
            None => ("follows main".to_owned(), Tok::Dim),
        };
        let entry = self.role_selection(role).and_then(|selection| {
            self.discovery
                .as_ref()?
                .models
                .iter()
                .find(|entry| same_model(selection, &entry.selection))
        });
        let caps_w = if entry.is_some() && w >= 28 { 8 } else { 0 };
        let value_w = w.saturating_sub(text::width(pointer) + 1 + 12 + caps_w);
        let value = text::ellipsize(&value, value_w, theme.ascii());
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
                    " ".repeat(value_w.saturating_sub(text::width(&value))),
                    fill,
                ),
                Span::styled(
                    entry
                        .map(|entry| capability_cell(theme, entry, caps_w))
                        .unwrap_or_default(),
                    Style::default().fg(Color::Reset).patch(fill),
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
                vec![Span::styled(
                    format!(" Assign {} to:", text::one_line(&chooser.name)),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                )],
                w,
                Style::default(),
            );
        }
        let Some((role, description)) = ROLES.get(row - 1) else {
            return filled(vec![], w, Style::default());
        };
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
                Span::styled((*description).to_owned(), theme.fg(Tok::Muted).patch(fill)),
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
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD),
        )];
        first.push(Span::styled(
            format!("  {}", capability_detail(theme, entry)),
            Style::default().fg(Color::Reset),
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
        let second = vec![
            Span::styled(format!(" {used_text}"), theme.fg(Tok::Muted)),
            Span::styled(
                format!(
                    "  {} {}",
                    theme.sym(Sym::Dot),
                    lora_detail(entry, self.main_lora_for(entry))
                ),
                theme.fg(Tok::Muted),
            ),
            Span::styled(
                format!(
                    "  {} {}",
                    theme.sym(Sym::Dot),
                    text::one_line(&entry.source)
                ),
                theme.fg(Tok::Dim),
            ),
        ];
        (first, second)
    }

    fn footer(&self, keys: &Keymap) -> Vec<String> {
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
        if self.group == Group::Roles {
            return vec![
                hint(keys, Action::SelectConfirm, "pick model"),
                format!(
                    "{} roles",
                    pair_label(keys, Action::SelectUp, Action::SelectDown)
                ),
                "x clear".to_owned(),
                "F6 verify LoRA".to_owned(),
                hint(keys, Action::TabPrev, "providers"),
                hint(keys, Action::SelectCancel, "close"),
            ];
        }
        vec![
            hint(
                keys,
                Action::SelectConfirm,
                if self.target_role.is_some() {
                    "assign"
                } else {
                    "set main"
                },
            ),
            format!(
                "{} models",
                pair_label(keys, Action::SelectUp, Action::SelectDown)
            ),
            hint(keys, Action::TabPrev, "providers"),
            "type to search".to_owned(),
            format!(
                "{} kind",
                pair_label(keys, Action::TabPrev, Action::TabNext)
            ),
            "Ctrl+A role".to_owned(),
            "Ctrl+S scope".to_owned(),
            "F5 rescan".to_owned(),
            "F6 verify LoRA".to_owned(),
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
}

#[cfg(test)]
mod tests {
    use super::super::settings::testkit::*;
    use super::*;
    use crate::llm::model_config::UiConfig;

    fn entry(
        provider: &str,
        model: &str,
        source: &str,
        available: bool,
        context: Option<u32>,
    ) -> ModelEntry {
        ModelEntry {
            selection: ModelSelection {
                provider: provider.into(),
                model: model.into(),
                base_url: Some(format!("http://{provider}/v1")),
                context_window: context,
                ..ModelSelection::default()
            },
            label: model.into(),
            source: source.into(),
            available,
            metadata: Default::default(),
        }
    }

    fn provider(id: &str, label: &str, state: &str, models: usize) -> ProviderStatus {
        ProviderStatus {
            id: id.into(),
            label: label.into(),
            base_url: Some(format!("http://{id}/v1")),
            state: state.into(),
            detail: String::new(),
            models,
        }
    }

    fn discovery() -> Discovery {
        Discovery {
            models: vec![
                entry("ollama", "llama3", "Ollama", true, None),
                entry(
                    "lm-studio",
                    "qwen-chat",
                    "LM Studio · loads on first use",
                    true,
                    None,
                ),
                entry(
                    "lm-studio",
                    "qwen-warm",
                    "LM Studio · loaded",
                    true,
                    Some(32768),
                ),
                entry(
                    "lm-studio",
                    "nomic-embed",
                    "LM Studio · embedding model, not for chat",
                    false,
                    Some(2048),
                ),
                entry(
                    "openai",
                    "gpt-cached",
                    "OpenAI (cached, unverified)",
                    false,
                    None,
                ),
            ],
            providers: vec![
                provider("lm-studio", "LM Studio", "reachable", 3),
                provider("ollama", "Ollama", "reachable", 1),
                provider("openai", "OpenAI", "auth-required", 1),
                provider("llama-cpp", "llama.cpp", "unavailable", 0),
            ],
        }
    }

    fn roles_with_main() -> Vec<(String, Option<ModelSelection>)> {
        let main = discovery().models[0].selection.clone();
        vec![
            ("main".into(), Some(main)),
            ("feedback".into(), None),
            ("subagent".into(), None),
            ("compaction".into(), None),
        ]
    }

    fn overlay() -> ModelOverlay {
        ModelOverlay::new(Some(discovery()), roles_with_main(), Scope::Global)
    }

    fn draw(o: &mut ModelOverlay, rig: &Rig, w: u16, h: u16) -> String {
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &|ui: &UiConfig, w| status_preview(ui, w),
        };
        screen(&render(w, h, |f| o.draw(f, &ctx)))
    }

    fn send(o: &mut ModelOverlay, rig: &Rig, e: Event) -> OverlayEvent {
        o.handle(&e, &rig.keys)
    }

    fn names(o: &ModelOverlay) -> Vec<String> {
        o.rows().iter().map(|(e, _)| e.label.clone()).collect()
    }

    #[test]
    fn capability_icons_labels_and_search_use_metadata_in_every_glyph_preset() {
        use crate::ui::theme::Glyphs;
        for glyphs in [Glyphs::Unicode, Glyphs::Nerd, Glyphs::Ascii] {
            let mut rig = Rig::new();
            rig.theme = rig.theme.with_glyphs(glyphs);
            let mut model = entry("mlx", "qwen-local", "Downloaded", true, Some(32768));
            model.metadata.vision = Some(true);
            model.metadata.tools = Some(true);
            model.metadata.reasoning = Some(true);
            let mut o = ModelOverlay::new(
                Some(Discovery {
                    models: vec![model.clone()],
                    providers: vec![],
                }),
                vec![],
                Scope::Global,
            );
            let shown = draw(&mut o, &rig, 150, 24);
            let row = shown
                .lines()
                .find(|line| line.contains("mlx/qwen-local"))
                .unwrap();
            for icon in [Sym::IconVision, Sym::IconTools, Sym::IconReasoning] {
                assert!(row.contains(rig.theme.sym(icon)), "{shown}");
            }
            o.roles = vec![("main".into(), Some(model.selection.clone()))];
            for selected in [false, true] {
                let line = o.model_line(&rig.theme, &model, &[], selected, 100);
                let icons = line.spans.last().unwrap();
                assert_eq!(icons.content, capability_cell(&rig.theme, &model, 8));
                assert_eq!(icons.style.fg, Some(Color::Reset));
                assert_eq!(line.width(), 100);
                assert_eq!(
                    line.spans[..line.spans.len() - 1]
                        .iter()
                        .map(|span| span.width())
                        .sum::<usize>(),
                    92
                );
            }
            let role = o.role_line(&rig.theme, 0, 100);
            assert_eq!(
                role.spans.last().unwrap().content,
                capability_cell(&rig.theme, &model, 8)
            );
            assert_eq!(role.spans.last().unwrap().style.fg, Some(Color::Reset));
            assert!(
                shown.contains("vision") && shown.contains("tools") && shown.contains("reasoning")
            );
            o.type_text("vision");
            assert_eq!(names(&o), ["qwen-local"]);
            model.metadata.tools = Some(false);
            model.metadata.reasoning = None;
            let detail = capability_detail(&rig.theme, &model);
            assert!(detail.contains("tools no") && detail.contains("reasoning unknown"));
            assert!(!capability_badges(&rig.theme, &model).contains(rig.theme.sym(Sym::IconTools)));
            let embed = entry("ollama", "embed-model", "embedding", false, None);
            assert_eq!(
                capability_badges(&rig.theme, &embed),
                rig.theme.sym(Sym::IconEmbedding)
            );
            for w in [28, 40, 60, 80, 100] {
                assert!(o.model_line(&rig.theme, &model, &[], true, w).width() <= w);
            }
        }
    }

    #[test]
    fn rows_and_details_show_lora_compatibility_and_the_main_adapter() {
        let rig = Rig::new();
        let local = |label: &str, lora| ModelEntry {
            selection: ModelSelection {
                model: format!("/models/{label}"),
                ..ModelSelection::default()
            },
            label: label.into(),
            source: "LM Studio download (MLX)".into(),
            available: true,
            metadata: crate::llm::catalog::ModelMetadata {
                lora,
                ..Default::default()
            },
        };
        let models = vec![
            local("qwen-mlx", Some(true)),
            local("gemma-mlx", Some(false)),
            entry("lm-studio", "qwen-served", "LM Studio · loaded", true, None),
        ];
        let main = models[0].selection.clone();
        let roles = vec![("main".into(), Some(main))];
        let mut o = ModelOverlay::new(
            Some(Discovery {
                models,
                providers: vec![],
            }),
            roles,
            Scope::Global,
        )
        .with_main_lora(Lora::Active(4));
        let shown = draw(&mut o, &rig, 130, 30);
        let row = |name: &str| {
            shown
                .lines()
                .find(|l| l.contains(name))
                .unwrap_or_else(|| panic!("{name} missing:\n{shown}"))
                .to_owned()
        };
        assert!(
            row("mlx/qwen-mlx").contains("accepted v4"),
            "the main model shows its admitted adapter"
        );
        assert!(row("mlx/gemma-mlx").contains("unsupported"));
        assert!(
            row("lm-studio/qwen-served").contains(" - "),
            "a served model has no LoRA column value"
        );
        assert!(
            shown.contains("LoRA v4 accepted; loading not confirmed"),
            "{shown}"
        );
        o.main_lora = Lora::Unsupported;
        let rejected = draw(&mut o, &rig, 130, 30);
        assert!(
            rejected
                .lines()
                .find(|line| line.contains("mlx/qwen-mlx"))
                .unwrap()
                .contains("unsupported")
        );
        assert!(rejected.contains("LoRA unavailable in Dial's current trainer"));
        o.main_lora = Lora::Unknown;
        let unchecked = draw(&mut o, &rig, 130, 30);
        assert!(
            unchecked
                .lines()
                .find(|line| line.contains("mlx/qwen-mlx"))
                .unwrap()
                .contains("unverified")
        );
        assert!(unchecked.contains("training targets have not been verified"));
        // Loaded models rank above merely available ones, so the served model is the second row.
        send(&mut o, &rig, key(KeyCode::Down));
        let shown = draw(&mut o, &rig, 130, 30);
        assert!(shown.contains("no LoRA: served by lm-studio"), "{shown}");
    }

    #[test]
    fn lora_verification_uses_highlighted_local_model_without_assigning_it() {
        let rig = Rig::new();
        let selection = ModelSelection {
            model: "/models/local-test".into(),
            ..Default::default()
        };
        let model = ModelEntry {
            selection: selection.clone(),
            label: "local-test".into(),
            source: "local MLX".into(),
            available: true,
            metadata: crate::llm::catalog::ModelMetadata {
                lora: Some(true),
                ..Default::default()
            },
        };
        let mut o = ModelOverlay::new(
            Some(Discovery {
                models: vec![model.clone()],
                providers: vec![],
            }),
            vec![],
            Scope::Global,
        );
        o.group = Group::All;
        o.focus = Focus::List;
        assert!(
            matches!(send(&mut o, &rig, key(KeyCode::F(6))), OverlayEvent::VerifyLora(found) if found.model == selection.model)
        );
        let mut verified = model;
        verified.metadata.lora_verified = true;
        assert_eq!(lora_cell(&verified, None).0, "verified");
        verified.metadata.lora = Some(false);
        assert_eq!(lora_cell(&verified, None).0, "unsupported");
        o.discovery.as_mut().unwrap().models[0] =
            entry("lm-studio", "remote", "loaded", true, None);
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::F(6))),
            OverlayEvent::None
        ));
    }

    #[test]
    fn left_column_shows_roles_all_models_and_providers_with_counts_and_state_glyphs() {
        let rig = Rig::new();
        let mut o = overlay();
        let shown = draw(&mut o, &rig, 110, 30);
        for needle in [
            "Models",
            "scope: global",
            "Roles",
            "All models",
            "LM Studio",
            "Ollama",
            "OpenAI",
        ] {
            assert!(shown.contains(needle), "missing {needle:?}\n{shown}");
        }
        let line = |n: &str| shown.lines().find(|l| l.contains(n)).unwrap().to_owned();
        assert!(
            line("All models")
                .trim_end_matches('│')
                .trim_end()
                .ends_with('5')
                || line("All models").contains(" 5"),
            "{}",
            line("All models")
        );
        assert!(
            line("LM Studio").contains('●') && line("LM Studio").contains('3'),
            "{}",
            line("LM Studio")
        );
        assert!(
            line("OpenAI").contains('⚠'),
            "auth-required warns: {}",
            line("OpenAI")
        );
        assert!(!shown.contains("llama.cpp"), "empty providers are hidden");
    }

    #[test]
    fn empty_providers_are_not_navigable_and_refresh_leaves_an_empty_group() {
        let mut o = overlay();
        assert!(!o.groups().contains(&Group::Provider("llama-cpp".into())));
        o.group = Group::Provider("ollama".into());
        let mut d = discovery();
        d.models.retain(|m| m.selection.provider != "ollama");
        d.providers
            .iter_mut()
            .find(|p| p.id == "ollama")
            .unwrap()
            .models = 0;
        o.set_discovery(d);
        assert_eq!(o.group, Group::All);
        assert!(!o.groups().contains(&Group::Provider("ollama".into())));
        assert!(o.groups().contains(&Group::Provider("lm-studio".into())));
    }

    #[test]
    fn right_pane_orders_main_first_then_loaded_then_available_then_unavailable() {
        let o = overlay();
        assert_eq!(
            names(&o),
            [
                "llama3",
                "qwen-warm",
                "qwen-chat",
                "nomic-embed",
                "gpt-cached"
            ]
        );
        let rig = Rig::new();
        let mut o = overlay();
        let shown = draw(&mut o, &rig, 120, 30);
        let row = shown
            .lines()
            .find(|l| l.contains("lm-studio/qwen-warm"))
            .unwrap();
        assert!(row.contains("32k") && row.contains("loaded"), "{row}");
        let row = shown
            .lines()
            .find(|l| l.contains("lm-studio/qwen-chat"))
            .unwrap();
        assert!(
            row.contains("loads on first use") && row.contains(" - "),
            "unknown context is '-': {row}"
        );
        let row = shown.lines().find(|l| l.contains("nomic-embed")).unwrap();
        assert!(row.contains("embedding"), "{row}");
        assert!(
            shown
                .lines()
                .find(|l| l.contains("ollama/llama3"))
                .unwrap()
                .contains('●'),
            "main model is marked"
        );
    }

    #[test]
    fn detail_lines_name_context_state_and_roles_using_the_model() {
        let rig = Rig::new();
        let mut roles = roles_with_main();
        roles[2].1 = Some(discovery().models[0].selection.clone());
        let mut o = ModelOverlay::new(Some(discovery()), roles, Scope::Global);
        let shown = draw(&mut o, &rig, 130, 30);
        assert!(shown.contains("used by: main, subagent"), "{shown}");
        send(&mut o, &rig, key(KeyCode::Down));
        let shown = draw(&mut o, &rig, 130, 30);
        assert!(
            shown.contains("context 32k")
                && shown.contains("not used by any role")
                && !shown.contains("Source"),
            "{shown}"
        );
    }

    #[test]
    fn typing_filters_fuzzily_and_highlights_matches() {
        let rig = Rig::new();
        let mut o = overlay();
        for e in typed("qw") {
            send(&mut o, &rig, e);
        }
        assert_eq!(names(&o), ["qwen-warm", "qwen-chat"]);
        let buf = render(120, 30, |f| {
            let c = DrawCtx {
                theme: &rig.theme,
                keys: &rig.keys,
                status: &rig.status,
                preview_status: &|u: &UiConfig, w| status_preview(u, w),
            };
            o.draw(f, &c)
        });
        let plain = style_of(&buf, "lm-studio/").unwrap();
        let matched = style_of(&buf, "qwen-warm").unwrap();
        assert_ne!(
            plain.fg,
            matched.fg,
            "matched characters are emphasized: {plain:?} vs {matched:?}\n{}",
            screen(&buf)
        );
        assert!(screen(&buf).contains(&format!("{} qw", rig.theme.sym(Sym::Prompt))));
        send(&mut o, &rig, key(KeyCode::Backspace));
        send(&mut o, &rig, key(KeyCode::Backspace));
        assert_eq!(names(&o).len(), 5);
        for e in typed("zzzz") {
            send(&mut o, &rig, e);
        }
        assert!(draw(&mut o, &rig, 120, 30).contains("No models match \"zzzz\""));
        assert!(
            matches!(send(&mut o, &rig, key(KeyCode::Esc)), OverlayEvent::None),
            "first Esc clears the search"
        );
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Esc)),
            OverlayEvent::Close
        ));
    }

    #[test]
    fn kind_filter_separates_chat_and_embedding_models() {
        let rig = Rig::new();
        let mut o = overlay();
        send(&mut o, &rig, key(KeyCode::Right));
        assert_eq!(o.kind, KindFilter::Chat);
        assert_eq!(
            names(&o),
            ["llama3", "qwen-warm", "qwen-chat", "gpt-cached"]
        );
        send(&mut o, &rig, key(KeyCode::Right));
        assert_eq!(names(&o), ["nomic-embed"]);
        send(&mut o, &rig, key(KeyCode::Right));
        assert_eq!(o.kind, KindFilter::Embedding, "clamps at the last kind");
        send(&mut o, &rig, key(KeyCode::Left));
        send(&mut o, &rig, key(KeyCode::Left));
        assert_eq!(o.kind, KindFilter::All);
        send(&mut o, &rig, key(KeyCode::Left));
        assert_eq!(
            o.focus,
            Focus::Groups,
            "Left from the first kind goes to the providers"
        );
    }

    #[test]
    fn provider_group_filters_and_groups_are_walked_with_the_keymap() {
        let rig = Rig::new();
        let mut o = overlay();
        send(&mut o, &rig, key(KeyCode::Tab));
        assert_eq!(o.focus, Focus::Groups);
        send(&mut o, &rig, key(KeyCode::Down));
        assert_eq!(o.group, Group::Provider("lm-studio".into()));
        assert_eq!(names(&o), ["qwen-warm", "qwen-chat", "nomic-embed"]);
        send(&mut o, &rig, key(KeyCode::Down));
        assert_eq!(names(&o), ["llama3"]);
        send(&mut o, &rig, key(KeyCode::Up));
        send(&mut o, &rig, key(KeyCode::Up));
        assert_eq!(o.group, Group::All);
        send(&mut o, &rig, key(KeyCode::Up));
        assert_eq!(o.group, Group::Roles);
        send(&mut o, &rig, key(KeyCode::Up));
        assert_eq!(o.group, Group::Roles, "stops at the top");
    }

    #[test]
    fn enter_assigns_main_with_the_active_scope_and_ctrl_s_changes_it() {
        let rig = Rig::new();
        let mut o = overlay();
        send(&mut o, &rig, key(KeyCode::Down));
        match send(&mut o, &rig, key(KeyCode::Enter)) {
            OverlayEvent::AssignModel {
                role,
                selection,
                scope,
            } => {
                assert_eq!(
                    (role.as_str(), selection.model.as_str(), scope),
                    ("main", "qwen-warm", Scope::Global)
                );
                assert_eq!(selection.context_window, Some(32768));
            }
            other => panic!("{other:?}"),
        }
        send(&mut o, &rig, ctrl('s'));
        assert!(draw(&mut o, &rig, 120, 30).contains("scope: project"));
        match send(&mut o, &rig, key(KeyCode::Enter)) {
            OverlayEvent::AssignModel { scope, .. } => assert_eq!(scope, Scope::Project),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unavailable_models_refuse_with_a_visible_reason() {
        let rig = Rig::new();
        let mut o = overlay();
        for _ in 0..3 {
            send(&mut o, &rig, key(KeyCode::Down));
        }
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        let shown = draw(&mut o, &rig, 130, 30);
        assert!(
            shown.contains("Cannot select lm-studio/nomic-embed")
                && shown.contains("embedding model, not for chat"),
            "{shown}"
        );
        let style = style_of(
            &render(130, 30, |f| {
                let c = DrawCtx {
                    theme: &rig.theme,
                    keys: &rig.keys,
                    status: &rig.status,
                    preview_status: &|u: &UiConfig, w| status_preview(u, w),
                };
                o.draw(f, &c)
            }),
            "Cannot select",
        )
        .unwrap();
        assert_eq!(style.fg, rig.theme.fg(Tok::Error).fg);
        // The message goes away on the next key, and unverified cached rows say so.
        send(&mut o, &rig, key(KeyCode::Down));
        assert!(!draw(&mut o, &rig, 130, 30).contains("Cannot select"));
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(
            o.message
                .as_ref()
                .unwrap()
                .0
                .contains("provider is not reachable")
        );
        assert!(matches!(send(&mut o, &rig, ctrl('a')), OverlayEvent::None));
        assert!(
            o.chooser.is_none(),
            "the role chooser refuses unavailable models too"
        );
    }

    #[test]
    fn ctrl_a_opens_the_role_chooser_and_assigns_that_role() {
        let rig = Rig::new();
        let mut o = overlay();
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, ctrl('a'));
        let shown = draw(&mut o, &rig, 120, 30);
        assert!(
            shown.contains("Assign lm-studio/qwen-warm to:")
                && shown.contains("feedback")
                && shown.contains("compaction"),
            "{shown}"
        );
        send(&mut o, &rig, key(KeyCode::Down));
        match send(&mut o, &rig, key(KeyCode::Enter)) {
            OverlayEvent::AssignModel {
                role, selection, ..
            } => assert_eq!(
                (role.as_str(), selection.model.as_str()),
                ("feedback", "qwen-warm")
            ),
            other => panic!("{other:?}"),
        }
        assert!(o.chooser.is_none());
        send(&mut o, &rig, ctrl('a'));
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Esc)),
            OverlayEvent::None
        ));
        assert!(
            o.chooser.is_none(),
            "Esc cancels the chooser, not the panel"
        );
    }

    #[test]
    fn roles_view_lists_roles_picks_models_and_clears() {
        let rig = Rig::new();
        let mut o = overlay();
        o.show_roles();
        let shown = draw(&mut o, &rig, 120, 30);
        assert!(
            shown.contains("main")
                && shown.contains("ollama/llama3")
                && shown.contains("follows main"),
            "{shown}"
        );
        assert!(
            shown.contains("architecture"),
            "the maintenance model role must be discoverable: {shown}"
        );
        send(&mut o, &rig, key(KeyCode::Down));
        // Pick a model for `feedback`.
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        let shown = draw(&mut o, &rig, 130, 30);
        assert!(
            shown.contains("choose the model for role \"feedback\""),
            "{shown}"
        );
        send(&mut o, &rig, key(KeyCode::Down));
        match send(&mut o, &rig, key(KeyCode::Enter)) {
            OverlayEvent::AssignModel {
                role, selection, ..
            } => assert_eq!(
                (role.as_str(), selection.model.as_str()),
                ("feedback", "qwen-warm")
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(o.group, Group::Roles, "back on the roles list");
        // Esc while picking returns to the roles list instead of closing.
        send(&mut o, &rig, key(KeyCode::Enter));
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Esc)),
            OverlayEvent::None
        ));
        assert_eq!(o.group, Group::Roles);
        // The controller reports the new assignment.
        let mut roles = roles_with_main();
        roles[1].1 = Some(discovery().models[2].selection.clone());
        o.set_roles(roles);
        assert!(draw(&mut o, &rig, 120, 30).contains("lm-studio/qwen-warm"));
        o.cursor = 1;
        match send(&mut o, &rig, ch('x')) {
            OverlayEvent::ClearRole { role, scope } => {
                assert_eq!((role.as_str(), scope), ("feedback", Scope::Global))
            }
            other => panic!("{other:?}"),
        }
        o.cursor = 2;
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Delete)),
            OverlayEvent::None
        ));
        assert!(draw(&mut o, &rig, 120, 30).contains("subagent already follows the main model"));
        o.cursor = 0;
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Delete)),
            OverlayEvent::None
        ));
        assert!(draw(&mut o, &rig, 130, 30).contains("main model cannot be cleared"));
    }

    #[test]
    fn discovery_arriving_later_keeps_search_cursor_and_group() {
        let rig = Rig::new();
        let mut o = ModelOverlay::new(None, roles_with_main(), Scope::Global);
        assert!(draw(&mut o, &rig, 120, 30).contains("Scanning providers and models"));
        let mut early = discovery();
        early.models.truncate(3);
        o.set_discovery(early);
        for e in typed("qwen") {
            send(&mut o, &rig, e);
        }
        send(&mut o, &rig, key(KeyCode::Down));
        assert_eq!(o.highlighted().unwrap().label, "qwen-chat");
        let mut later = discovery();
        later.models.insert(
            0,
            entry(
                "lm-studio",
                "qwen-new",
                "LM Studio · loaded",
                true,
                Some(4096),
            ),
        );
        o.set_discovery(later);
        assert_eq!(o.search, "qwen");
        assert_eq!(
            o.highlighted().unwrap().label,
            "qwen-chat",
            "the same model stays highlighted even though rows were added above it"
        );
        // A group that disappears falls back to All.
        send(&mut o, &rig, key(KeyCode::Esc));
        send(&mut o, &rig, key(KeyCode::Tab));
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, key(KeyCode::Down));
        assert_eq!(o.group, Group::Provider("openai".into()));
        let mut smaller = discovery();
        smaller.providers.retain(|p| p.id != "openai");
        o.set_discovery(smaller);
        assert_eq!(o.group, Group::All);
    }

    #[test]
    fn kind_legend_omits_whole_items_when_space_is_limited() {
        let o = ModelOverlay::new(Some(Discovery::default()), vec![], Scope::Global);
        let theme = Theme::dark();
        let full = o.kind_line(&theme, 200);
        let full: String = full.spans.iter().map(|s| s.content.as_ref()).collect();
        let unknown = full.find("  ? unknown").unwrap();
        let width = text::width(&full[..unknown]) + 5;
        let narrow = o.kind_line(&theme, width);
        let narrow: String = narrow.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!narrow.contains('?'), "{narrow}");
        assert!(text::width(&narrow) <= width);
        assert!(full.contains("? unknown"));
    }

    #[test]
    fn empty_states_explain_themselves() {
        let rig = Rig::new();
        let mut o = ModelOverlay::new(Some(Discovery::default()), vec![], Scope::Global);
        let shown = draw(&mut o, &rig, 120, 30);
        assert!(shown.contains("No models found"), "{shown}");
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Down)),
            OverlayEvent::None
        ));
        let mut o = overlay();
        send(&mut o, &rig, key(KeyCode::Right));
        send(&mut o, &rig, key(KeyCode::Right));
        send(&mut o, &rig, key(KeyCode::Tab));
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, key(KeyCode::Down));
        send(&mut o, &rig, key(KeyCode::Tab));
        assert!(draw(&mut o, &rig, 120, 30).contains("No models in this view"));
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
    }

    #[test]
    fn remapped_keys_drive_the_selector_and_the_footer() {
        let mut rig = Rig::new();
        rig.keys.rebind(Action::SelectDown, &["Ctrl+J"]).unwrap();
        rig.keys.rebind(Action::SelectConfirm, &["Ctrl+O"]).unwrap();
        let mut o = overlay();
        send(&mut o, &rig, key(KeyCode::Down));
        assert_eq!(o.cursor, 0, "old chord is unbound");
        send(&mut o, &rig, ctrl('j'));
        assert_eq!(o.cursor, 1);
        assert!(matches!(
            send(&mut o, &rig, ctrl('o')),
            OverlayEvent::AssignModel { .. }
        ));
        assert!(draw(&mut o, &rig, 160, 30).contains("Ctrl+O set main"));
    }

    #[test]
    fn refresh_keys_ask_for_a_rescan() {
        let rig = Rig::new();
        let mut o = overlay();
        assert!(matches!(
            send(&mut o, &rig, key(KeyCode::F(5))),
            OverlayEvent::Refresh
        ));
    }

    #[test]
    fn tiny_terminals_never_panic() {
        let rig = Rig::new();
        for w in 0..=30u16 {
            for h in 0..=30u16 {
                let mut o = overlay();
                draw(&mut o, &rig, w, h);
                send(&mut o, &rig, key(KeyCode::Down));
                send(&mut o, &rig, ctrl('a'));
                draw(&mut o, &rig, w, h);
                send(&mut o, &rig, key(KeyCode::Esc));
                o.show_roles();
                draw(&mut o, &rig, w, h);
                for e in typed("qwen") {
                    send(&mut o, &rig, e);
                }
                draw(&mut o, &rig, w, h);
                let mut none = ModelOverlay::new(None, vec![], Scope::Project);
                draw(&mut none, &rig, w, h);
            }
        }
    }

    #[test]
    fn long_and_hostile_model_names_stay_inside_their_columns() {
        let rig = Rig::new();
        let mut d = discovery();
        d.models.push(entry(
            "lm-studio",
            "日本語のとても長いモデル名\u{1b}[31mred",
            "LM Studio",
            true,
            Some(8192),
        ));
        d.models.push(entry(
            "lm-studio",
            &"x".repeat(300),
            "LM Studio",
            true,
            None,
        ));
        let mut o = ModelOverlay::new(Some(d), roles_with_main(), Scope::Global);
        for w in [40u16, 70, 120] {
            let shown = draw(&mut o, &rig, w, 24);
            assert!(!shown.contains('\u{1b}'));
            for line in shown.lines() {
                assert!(text::width(line) <= usize::from(w) * 2, "{line}");
            }
        }
    }
}

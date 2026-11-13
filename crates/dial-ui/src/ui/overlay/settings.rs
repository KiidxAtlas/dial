//! The `/settings` panel: tab bar, section column, grouped value list, description, live status-line
//! preview and key hints. It never touches configuration: every change leaves as an
//! [`OverlayEvent`] and the displayed value only changes once the controller answers via
//! [`SettingsOverlay::settled`].
//!
//! This file also holds the small drawing helpers shared by the model and provider panels.
use super::path_browser::PathBrowser;
use super::{DrawCtx, OverlayEvent};
use crate::config::schema::{Kind, PathKind, SettingDef, Source};
use crate::config::settings_store::Scope;
use crate::llm::model_config::{ModelSelection, UiConfig};
use crate::ui::chrome::Section;
use crate::ui::editor::Editor;
use crate::ui::keys::{Action, Keymap};
use crate::ui::text;
use crate::ui::theme::{Sym, Tok};
use crossterm::event::{Event, KeyCode};
#[cfg(test)]
use crossterm::event::{KeyEvent, KeyModifiers};
#[cfg(test)]
use format::{adjust, blank_meaning, edit_text};
use format::{choice_value, parse_input, setting_edit_text, value_text};
use memory::{label_for_choice, memory_needs, memory_section};
use ratatui::{
    Frame,
    text::{Line, Span},
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use toml::{Table, Value};
use unicode_segmentation::UnicodeSegmentation;
use value_editors::{AddBy, EntriesEditor, ListEditor};
pub(crate) use widgets::scroll_cell;
pub(super) use widgets::{
    Axis, arrows, editor_spans, filled, fit, highlighted, hint, hint_line, hint_line_after,
    is_ctrl, list_row, local_function_key, local_function_label, match_style, other_scope,
    pair_label, panel, pressed, scope_caption, scope_name, tab_strip, typed_char,
};

mod appearance;
mod bulk;
mod context;
mod embedding;
mod format;
mod input;
mod memory;
mod render;
mod value_editors;
mod widgets;

/// Bulk removal is limited to canonical leaf settings; section containers can hide siblings.
pub fn resettable_path(path: &str) -> bool {
    let defs = crate::config::schema::registry();
    defs.iter().any(|def| def.path == path)
        && !defs.iter().any(|def| {
            def.path
                .strip_prefix(path)
                .is_some_and(|tail| tail.starts_with('.'))
        })
}

#[cfg(test)]
pub(super) mod testkit;

// ---------------------------------------------------------------------------------------------
// Settings panel
// ---------------------------------------------------------------------------------------------

/// Choices a picker shows at once; longer lists get a filter line.
const PICK_ROWS: usize = 7;
/// The most choices an open picker lists at once, however tall the panel.
const MAX_PICK_ROWS: usize = 12;

/// Indexes of the `choices` the filter keeps, in order. Each space-separated term must appear in
/// the choice's label or raw value, ignoring case, so "auto" finds "Auto (let Dial decide)".
fn pick_matches(labels: &[String], choices: &[String], filter: &str) -> Vec<usize> {
    let terms: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    (0..choices.len())
        .filter(|&n| {
            // Internal rows (offers, "type another value") match by label only.
            let raw = if choices[n].starts_with('\u{1}') {
                ""
            } else {
                choices[n].as_str()
            };
            let haystack = format!("{} {raw}", labels[n]).to_lowercase();
            terms.iter().all(|term| haystack.contains(term.as_str()))
        })
        .collect()
}

/// Picker row that leaves for the plain text editor; never a saved value.
const TYPE_CHOICE: &str = "\u{1}type";
/// Prefix of picker rows standing for `value_choices[path][index]`.
const OFFER_PREFIX: &str = "\u{1}offer:";

const PROVIDERS_TAB: &str = "Setup";
const LINK_SECTION: &str = "Models, connections & controls";
/// Launchers for dedicated setup panels: (label, description, application action).
const LINKS: &[(&str, &str, &str)] = &[
    (
        "Providers & API keys",
        "Add or remove providers, see which are reachable and manage API keys.",
        "open:providers",
    ),
    (
        "Models & roles",
        "Choose the main model and models for feedback, subagents, compaction, architecture and escalation.",
        "open:models",
    ),
    (
        "Keyboard shortcuts",
        "Search, edit or restore keyboard controls. Shortcut changes apply to all projects.",
        "open:shortcuts",
    ),
    (
        "Compute & background work",
        "See compute owners across projects; pause, resume, start or stop improvement jobs.",
        "open:compute",
    ),
    (
        "Tool servers & extensions",
        "Connect tool servers and manage installed extensions in their dedicated panel.",
        "open:extensions",
    ),
    (
        "Context defaults",
        "Set shared context defaults for future model selections; current model tuning stays separate.",
        "open:context-defaults",
    ),
    (
        "Extraction model",
        "Choose the memory extraction model in its existing role picker; this does not enable memory.",
        "open:role:memory",
    ),
    (
        "Embedding model",
        "Choose the embedding model in its existing role picker; this does not enable semantic memory.",
        "open:role:embedding",
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Setting(usize),
    Link(usize),
    /// Toggle for a section's advanced settings: (tab, section, number hidden).
    Advanced(&'static str, &'static str, usize),
}

enum Pending {
    Set(Value, Scope),
    Unset(Scope),
}

enum Mode {
    Browse,
    Edit {
        def: &'static SettingDef,
        editor: Editor,
    },
    Pick {
        def: &'static SettingDef,
        choices: Vec<String>,
        cursor: usize,
        /// Type-to-filter text. Matches a choice's label or raw value, ignoring case.
        filter: Editor,
    },
    /// A file or folder browser for a path setting.
    Path {
        def: &'static SettingDef,
        browser: Box<PathBrowser>,
    },
    /// Check, order and add values of a list setting.
    List {
        def: &'static SettingDef,
        editor: Box<ListEditor>,
    },
    /// A TOML map or array edited one entry at a time.
    Entries {
        def: &'static SettingDef,
        editor: Box<EntriesEditor>,
    },
}

enum Item {
    Header(&'static str),
    Row(usize),
    /// A save or validation error under a row. Shown in red.
    Note(String),
    /// A read-only finding under a row (for example a program that is not on PATH). Muted, never red.
    Hint(String),
    /// A dependency-setup line: the first line of an offer carries the `missing` chip.
    Setup {
        line: String,
        chip: bool,
    },
    Choice {
        name: String,
        on: bool,
    },
    /// The picker's filter line: its editor (drawn with its caret), choices kept, and choices total.
    Filter {
        editor: Editor,
        shown: usize,
        total: usize,
    },
    /// An open one-line input of a structured editor.
    Input {
        prompt: &'static str,
        editor: Editor,
    },
}

/// What choosing an offered value does.
#[derive(Clone, Debug, PartialEq)]
pub enum ChoiceAction {
    /// Save the value.
    Set,
    /// Download the local model the value names (a second Enter confirms), then save it.
    Download,
    /// Install and prepare a voice backend after explicit confirmation, then select it.
    Install { preview: String },
    /// A backend requiring manual credentials or platform support, not an installable package.
    Unavailable { reason: String },
    /// Assign the model to a role through the canonical selection writer.
    Assign {
        role: String,
        selection: ModelSelection,
    },
}

/// A value a setting can take, offered by the controller (discovered models, configured names).
#[derive(Clone, Debug, PartialEq)]
pub struct ValueChoice {
    pub value: String,
    pub label: String,
    pub action: ChoiceAction,
}

/// A missing dependency that a setting path needs: what to install and the plan to show first.
#[derive(Clone)]
pub struct SetupOffer {
    pub id: String,
    pub title: String,
    pub preview: String,
}

/// A discovered model the `embedding` role can take, listed on the Embedding model row.
pub struct EmbeddingModel {
    pub selection: ModelSelection,
    pub label: String,
    /// False for a model whose server did not say what it is (it may not be an embedding model).
    pub classified: bool,
}

/// The settings row that picks the `embedding` role (and holds the advanced model override).
const EMBEDDING_MODEL_PATH: &str = crate::setup::embeddings::EMBEDDING_MODEL_PATH;
const EMBEDDING_URL_PATH: &str = "memory.semantic.embedding_base_url";
/// Override rows kept behind "Show advanced" while no override URL is set.
const EMBEDDING_OVERRIDE_ROWS: [&str; 2] =
    [EMBEDDING_URL_PATH, "memory.semantic.embedding_api_key_env"];

pub struct SettingsOverlay {
    panel_keys: super::panel_keys::PanelKeys,
    effective: Table,
    /// Display-only differences from the actual active config. Sensitive values are never retained.
    application_changes: BTreeMap<&'static str, (String, String)>,
    application_busy: bool,
    project: Table,
    global: Table,
    sources: BTreeMap<String, &'static str>,
    scope: Scope,
    themes: Vec<String>,
    tabs: Vec<(&'static str, Vec<&'static str>)>,
    learning_only: bool,
    context_only: bool,
    tab: usize,
    cursor: usize,
    scroll: usize,
    page: usize,
    search: String,
    /// Saved overrides from the editing layer across categories, including folded rows.
    changed_only: bool,
    bulk_review: Option<bulk::Review>,
    project_name: String,
    mode: Mode,
    /// Presentation only: the Appearance preview never owns preference values.
    appearance_preview: bool,
    appearance_scroll: usize,
    reveal_sensitive: bool,
    pending: BTreeMap<String, Pending>,
    /// Save feedback is shown only after the controller acknowledges persistence.
    feedback: Option<String>,
    /// A controller read/setup failure must not masquerade as an empty settings layer.
    load_error: Option<String>,
    /// Inline messages under a row (validation errors, hints), by setting path.
    notes: BTreeMap<String, String>,
    /// Read-only findings under a row (for example whether a program is on PATH); not cleared by edits.
    hints: BTreeMap<String, String>,
    /// Missing-dependency offers by setting path; Ctrl+L shows the plan, then starts the install.
    setup_offers: BTreeMap<String, SetupOffer>,
    /// The dependency whose plan is showing; the next Ctrl+L on it starts the install.
    setup_armed: Option<String>,
    /// True while a setup job this overlay started is running; the controller clears it once it settles.
    setup_pending: bool,
    /// A memory backend the user chose whose required fields are still empty. Its section is
    /// shown so they can be filled; the switch happens once they are (see [`memory_needs`]).
    memory_intent: Option<String>,
    /// Sections whose advanced settings are expanded, by (tab, section).
    advanced_open: BTreeSet<(&'static str, &'static str)>,
    /// Discovered models the embedding role can take (supplied by the controller).
    embedding_models: Vec<EmbeddingModel>,
    /// Setup offers for the curated embedding models, by model name.
    embedding_setups: BTreeMap<String, SetupOffer>,
    /// One half of the embedding override pair, held until its partner is set so both save together.
    staged_pair: Option<(&'static str, Value)>,
    /// Offered values for model and name settings, by setting path (supplied by the controller).
    value_choices: BTreeMap<String, Vec<ValueChoice>>,
    /// Where path browsers start when a setting has no value.
    base_dir: PathBuf,
    /// The not-yet-downloaded model whose first Enter asked for confirmation.
    download_armed: Option<String>,
}

/// The tab of frequently changed settings (see `crate::config::schema::COMMON_SETTINGS`).
const COMMON_TAB: &str = "Common";

/// Settings people change often that belong to another tab, also listed under Common: the model,
/// how much Dial may do on its own, what Enter does mid-turn and when it asks for attention.
const COMMON_MIRRORS: &[&str] = &[
    "model",
    "approval",
    "sandbox.mode",
    "ui.busy_send",
    "ask.notify",
];

fn label_of(path: &str) -> &'static str {
    crate::config::schema::registry()
        .iter()
        .find(|d| d.path == path)
        .map_or("", |d| d.label)
}

fn haystack(def: &SettingDef) -> String {
    let (tab, section) = def.search_location();
    format!(
        "{} {} {} {} {} {} {}",
        def.label,
        section,
        tab,
        def.path,
        def.section,
        def.tab,
        def.search_terms()
    )
}

/// Fuzzy points a query character must average for a scattered (non-contiguous) match to count.
/// A run of consecutive characters earns 30 and more each; letters picked one by one from unrelated
/// words earn about 10 minus their gaps. The cut keeps abbreviations such as `sndbx` while dropping
/// rows that only contain the letters somewhere (`ui.theme` matching a Hindsight timeout).
const FUZZY_FLOOR: i32 = 15;

/// Match words independently, keeping fuzzy matching on concise metadata rather than long prose.
/// A word found whole in the metadata ranks above a fuzzy hit; a weak scattered hit falls back to
/// the description, and description-only hits do not invent highlights in the setting's label.
fn search_match(query: &str, metadata: &str, description: &str) -> Option<(i32, Vec<usize>)> {
    let description = description.to_lowercase();
    let lower: Vec<char> = metadata.to_lowercase().chars().collect();
    let mut score = 0;
    let mut positions = Vec::new();
    for word in query.split_whitespace() {
        let needle: Vec<char> = word.to_lowercase().chars().collect();
        let whole = (lower.len() == metadata.chars().count())
            .then(|| {
                lower
                    .windows(needle.len())
                    .position(|w| w == needle.as_slice())
            })
            .flatten();
        if let Some(start) = whole {
            score += 40 * needle.len() as i32 + if start == 0 { 30 } else { 0 };
            positions.extend(start..start + needle.len());
        } else if let Some((rank, matched)) = text::fuzzy(word, metadata)
            .filter(|(rank, _)| *rank >= FUZZY_FLOOR * needle.len() as i32)
        {
            score += rank;
            positions.extend(matched);
        } else if !description.contains(&word.to_lowercase()) {
            return None;
        }
    }
    positions.sort_unstable();
    positions.dedup();
    Some((score, positions))
}

impl SettingsOverlay {
    /// Focus a declared setting for contextual navigation. Clears local filters, expands advanced
    /// rows and leaves editing scope unchanged. Unknown paths or active editors return `false`
    /// without changes. Inactive backend rows use exact-path search, without activation.
    pub fn focus_path(&mut self, path: &str) -> bool {
        if !matches!(self.mode, Mode::Browse) {
            return false;
        }
        let Some(def) = crate::config::schema::registry()
            .iter()
            .find(|def| def.path == path)
        else {
            return false;
        };
        self.search.clear();
        self.changed_only = false;
        self.mode = Mode::Browse;
        self.learning_only = false;
        self.tabs = crate::config::schema::tabs();
        self.tab = self
            .tabs
            .iter()
            .position(|(tab, _)| *tab == def.tab)
            .unwrap_or(0);
        self.advanced_open.insert((def.tab, def.section));
        if !self.visible().iter().any(|row| matches!(row, Row::Setting(i) if crate::config::schema::registry()[*i].path == path)) {
            self.search = path.to_owned();
        }
        self.move_to_path(path);
        self.scroll = 0;
        true
    }
    pub fn with_sources(mut self, sources: BTreeMap<String, &'static str>) -> Self {
        self.sources = sources;
        self
    }

    pub fn new(
        values: Table,
        project_values: Table,
        global_values: Table,
        scope: Scope,
        themes: Vec<String>,
    ) -> Self {
        SettingsOverlay {
            panel_keys: Default::default(),
            effective: values,
            application_changes: BTreeMap::new(),
            application_busy: false,
            project: project_values,
            global: global_values,
            sources: BTreeMap::new(),
            scope,
            themes,
            tabs: crate::config::schema::tabs(),
            learning_only: false,
            context_only: false,
            tab: 0,
            cursor: 0,
            scroll: 0,
            page: 8,
            search: String::new(),
            changed_only: false,
            bulk_review: None,
            project_name: "this project".to_owned(),
            mode: Mode::Browse,
            appearance_preview: false,
            appearance_scroll: 0,
            reveal_sensitive: false,
            pending: BTreeMap::new(),
            notes: BTreeMap::new(),
            hints: BTreeMap::new(),
            setup_offers: BTreeMap::new(),
            setup_armed: None,
            setup_pending: false,
            memory_intent: None,
            advanced_open: BTreeSet::new(),
            embedding_models: Vec::new(),
            embedding_setups: BTreeMap::new(),
            staged_pair: None,
            value_choices: BTreeMap::new(),
            base_dir: std::env::current_dir().unwrap_or_default(),
            download_armed: None,
            feedback: None,
            load_error: None,
        }
    }

    /// Identify the project whose saved layer is being edited, without exposing its full path.
    pub fn with_project_name(mut self, name: &str) -> Self {
        self.project_name = text::one_line(name);
        self
    }

    fn editing_scope_label(&self) -> String {
        match self.scope {
            Scope::Global => "global (all projects)".to_owned(),
            Scope::Project => format!("project ({})", self.project_name),
        }
    }

    /// Compatibility entry point: settings includes persistent learning preferences too.
    pub fn coding_only(self) -> Self {
        self
    }
    pub fn learning_only(mut self) -> Self {
        self.learning_only = true;
        self.filter_tabs();
        self
    }

    /// Shared defaults for future model selections, rather than a current-model override.
    pub fn context_defaults(mut self) -> Self {
        self.context_only = true;
        self.filter_tabs();
        let def = crate::config::schema::registry()
            .iter()
            .find(|def| def.path == "local_context_window")
            .expect("local context setting is registered");
        self.tab = self
            .tabs
            .iter()
            .position(|(tab, _)| *tab == def.tab)
            .expect("model settings tab is visible");
        self.scope = Scope::Global;
        self.move_to_path(def.path);
        self
    }
    fn learning_setting(def: &SettingDef) -> bool {
        def.search_location().0 == "Improvement"
            || def.path.starts_with("laya.")
            || def.path.starts_with("memory.autolearn")
            || def.path.starts_with("memory.learn_")
    }
    fn scope_shown(&self, def: &SettingDef) -> bool {
        if self.context_only {
            return Self::context_setting(def);
        }
        // The one learning switch that changes coding sessions belongs in both views.
        if def.path == "improve.teach" {
            return true;
        }
        !self.learning_only || Self::learning_setting(def)
    }
    fn filter_tabs(&mut self) {
        self.tabs = crate::config::schema::tabs()
            .into_iter()
            .filter_map(|(tab, sections)| {
                let sections = sections
                    .into_iter()
                    .filter(|section| {
                        crate::config::schema::registry().iter().any(|def| {
                            def.tab == tab && def.section == *section && self.scope_shown(def)
                        })
                    })
                    .collect::<Vec<_>>();
                (!sections.is_empty()).then_some((tab, sections))
            })
            .collect();
    }

    /// Attach a read-only finding to a setting row, computed by the caller (the UI reads no config).
    pub fn set_hint(&mut self, path: &str, text: String) {
        self.hints.insert(path.to_owned(), text);
    }

    /// Attach missing-dependency offers (computed by the caller). Clears any shown plan.
    pub fn set_setup_offers(&mut self, offers: BTreeMap<String, SetupOffer>) {
        self.setup_offers = offers;
        self.setup_armed = None;
    }

    /// Whether a setup job started from this overlay is still running.
    pub fn setup_pending(&self) -> bool {
        self.setup_pending
    }

    /// Marks a setup job as started (or finished) by the controller.
    pub fn set_setup_pending(&mut self, pending: bool) {
        self.setup_pending = pending;
    }

    /// Discovered models the embedding role can take, listed on the Embedding model row.
    pub fn set_embedding_models(&mut self, models: Vec<EmbeddingModel>) {
        self.embedding_models = models;
    }

    /// Values the model and name settings offer, by setting path.
    pub fn set_value_choices(&mut self, choices: BTreeMap<String, Vec<ValueChoice>>) {
        self.value_choices = choices;
    }

    /// The folder path browsers start in when a setting has no value (the project root).
    pub fn set_base_dir(&mut self, dir: PathBuf) {
        self.base_dir = dir;
    }

    /// Setup offers for the curated embedding models, by model name; picking one on the Embedding
    /// model row makes Ctrl+L set up that model.
    pub fn set_embedding_setups(&mut self, offers: BTreeMap<String, SetupOffer>) {
        self.embedding_setups = offers;
    }

    /// The held half of the embedding override pair, to save together with its partner. The pending
    /// entry stays until the controller settles that path.
    pub fn take_staged_pair(&mut self) -> Option<(String, Value)> {
        self.staged_pair
            .take()
            .map(|(path, value)| (path.to_owned(), value))
    }

    pub fn replace_values(&mut self, values: Table, project_values: Table, global_values: Table) {
        self.effective = values;
        self.project = project_values;
        self.global = global_values;
        self.pending.clear();
        self.staged_pair = None;
        self.notes.clear();
        self.load_error = None;
    }

    /// Refresh a retained page without discarding its editor, staged pair or navigation state.
    pub fn refresh_values(
        &mut self,
        values: Table,
        project_values: Table,
        global_values: Table,
        sources: BTreeMap<String, &'static str>,
    ) {
        self.effective = values;
        self.project = project_values;
        self.global = global_values;
        self.sources = sources;
        self.load_error = None;
    }

    /// Compare saved values with the controller's immutable active snapshot, not a disk reload.
    /// Keep only rendered values (or hidden markers), leaving editors and navigation untouched.
    pub fn set_application_state(&mut self, active: &Table, busy: bool) {
        self.set_projected_application_state(active, None, busy);
    }

    /// An optional canonical runtime projection excludes mode-inactive saved preferences from
    /// pending adoption, without duplicating the controller's runtime-mode rules.
    pub fn set_projected_application_state(
        &mut self,
        active: &Table,
        projected: Option<&Table>,
        busy: bool,
    ) {
        self.application_busy = busy;
        self.application_changes.clear();
        for def in crate::config::schema::registry() {
            let default = def.default_value();
            let before = crate::config::schema::get(active, def.path).or(default.as_ref());
            let saved = crate::config::schema::get(&self.effective, def.path).or(default.as_ref());
            if !def.restart
                && !def.path.starts_with("ui.")
                && projected.is_some_and(|values| {
                    crate::config::schema::get(values, def.path).or(default.as_ref()) == before
                })
            {
                continue;
            }
            if before != saved {
                let shown = |value: Option<&Value>| {
                    value.map_or_else(|| "(not set)".into(), |value| value_text(def, value))
                };
                self.application_changes
                    .insert(def.path, (shown(before), shown(saved)));
            }
        }
    }

    pub fn pending_application_count(&self) -> usize {
        crate::config::schema::registry()
            .iter()
            .filter(|def| {
                !def.restart
                    && !def.path.starts_with("ui.")
                    && self.application_changes.contains_key(def.path)
            })
            .count()
    }

    fn application_timing(&self, def: &SettingDef) -> &'static str {
        if def.restart {
            "restart required"
        } else if def.path.starts_with("ui.") {
            "display preference; applies on panel close"
        } else if self.application_busy {
            "after this turn"
        } else {
            "next request"
        }
    }

    fn application_detail(&self, def: &SettingDef) -> Option<String> {
        self.application_changes
            .get(def.path)
            .map(|(active, saved)| {
                format!(
                    "Active {active} → Saved {saved} · {}",
                    self.application_timing(def)
                )
            })
    }

    /// A model role was assigned or cleared elsewhere (the Embedding model row, or a setup job):
    /// re-check a backend switch that waits on it.
    pub fn roles_changed(&mut self) {
        self.recheck_memory_intent();
    }

    /// Report background setup and settings-layer failures without replacing last-good values.
    pub fn banner(&mut self, message: String, is_error: bool) {
        if is_error {
            self.load_error = Some(text::one_line(&message));
        } else {
            self.feedback = Some(text::one_line(&message));
        }
    }

    /// The controller's verdict for the last `Set`/`Unset` of `path`.
    pub fn settled(&mut self, path: &str, result: Result<Value, String>) {
        let pending = self.pending.remove(path);
        self.feedback = None;
        match result {
            Ok(value) => {
                match pending {
                    Some(Pending::Set(v, scope)) => {
                        crate::config::schema::set(self.layer(scope), path, v);
                        self.feedback = Some(
                            if scope == Scope::Global
                                && crate::config::schema::get(&self.project, path).is_some()
                            {
                                "Saved globally; this project's override still applies".to_owned()
                            } else {
                                format!("Saved to {} settings", scope_name(scope))
                            },
                        );
                    }
                    Some(Pending::Unset(scope)) => {
                        crate::config::schema::unset(self.layer(scope), path);
                        self.feedback = Some(format!("Removed {} override", scope_name(scope)));
                    }
                    None => {}
                }
                crate::config::schema::set(&mut self.effective, path, value);
                if path == "memory.backend" {
                    self.memory_intent = None;
                }
                self.notes.remove(path);
                if let Some(intent) = self.memory_intent.as_deref()
                    && path.starts_with(memory_section(intent).unwrap_or("\0"))
                {
                    self.recheck_memory_intent();
                }
            }
            Err(message) => {
                self.notes.insert(path.to_owned(), text::one_line(&message));
            }
        }
    }

    /// Whether a model role is assigned in the effective settings.
    fn has_role(&self, role: &str) -> bool {
        crate::config::schema::get(&self.effective, &format!("model_roles.{role}")).is_some()
    }

    /// While a backend switch waits for its fields, point at the next one or say it is ready.
    fn recheck_memory_intent(&mut self) {
        let Some(intent) = self.memory_intent.clone() else {
            return;
        };
        match memory_needs(&intent, |p| self.current_path(p), |r| self.has_role(r)).first() {
            Some(next) => self.move_to_path(next),
            None => {
                self.move_to_path("memory.backend");
                self.feedback = Some(format!(
                    "{} is ready: press Enter on Backend to switch to it.",
                    label_for_choice(&intent)
                ));
            }
        }
    }

    /// Whether a row hides behind its section's "Show advanced" toggle. The embedding URL override
    /// rows do while no override URL is set.
    fn advanced(&self, path: &str) -> bool {
        crate::config::schema::is_advanced(path)
            || self.context_advanced(path)
            || (EMBEDDING_OVERRIDE_ROWS.contains(&path)
                && self
                    .current_path(EMBEDDING_URL_PATH)
                    .as_str()
                    .is_none_or(|url| url.trim().is_empty()))
    }

    fn layer(&mut self, scope: Scope) -> &mut Table {
        match scope {
            Scope::Global => &mut self.global,
            Scope::Project => &mut self.project,
        }
    }

    fn layer_ref(&self, scope: Scope) -> &Table {
        match scope {
            Scope::Global => &self.global,
            Scope::Project => &self.project,
        }
    }

    fn source(&self, path: &str) -> &'static str {
        if let Some(source @ ("env" | "CLI flag")) = self.sources.get(path).copied() {
            return source;
        }
        if crate::config::schema::get(&self.project, path).is_some() {
            "project"
        } else if crate::config::schema::get(&self.global, path).is_some() {
            "global"
        } else {
            "default"
        }
    }

    /// What `path` falls back to once it is removed from `scope`.
    fn after_unset(&self, def: &SettingDef, scope: Scope) -> Option<Value> {
        if matches!(self.source(def.path), "env" | "CLI flag") {
            return crate::config::schema::get(&self.effective, def.path).cloned();
        }
        crate::config::schema::get(
            self.layer_ref(super::settings::other_scope(scope)),
            def.path,
        )
        .cloned()
        .or_else(|| def.default_value())
    }

    /// Explain persistence and precedence without exposing raw sensitive values.
    fn source_detail(&self, def: &SettingDef) -> String {
        let source = self.source(def.path);
        let owned = crate::config::schema::get(self.layer_ref(self.scope), def.path).is_some();
        let action = if matches!(source, "env" | "CLI flag") {
            format!("{source} wins over saved settings; reset keeps this runtime value")
        } else if owned {
            let fallback_source =
                if crate::config::schema::get(self.layer_ref(other_scope(self.scope)), def.path)
                    .is_some()
                {
                    scope_name(other_scope(self.scope))
                } else {
                    "default"
                };
            let fallback = self
                .after_unset(def, self.scope)
                .map_or_else(|| "(not set)".to_owned(), |value| value_text(def, &value));
            format!("Delete: {fallback_source} ({fallback})")
        } else {
            format!("Inherited; edit adds a {} override", scope_name(self.scope))
        };
        let source_detail = format!(
            "Using {source} value · {action} · {}{}",
            def.path,
            if def.restart {
                " · Restart required"
            } else {
                ""
            }
        );
        match self.application_detail(def) {
            Some(timing) => format!("{timing} · {source_detail}"),
            None => source_detail,
        }
    }

    fn current_path(&self, path: &str) -> Value {
        crate::config::schema::registry()
            .iter()
            .find(|d| d.path == path)
            .map_or(Value::String(String::new()), |d| self.current(d))
    }

    fn move_to_path(&mut self, path: &str) {
        let find = |rows: Vec<Row>| {
            rows.iter().position(
                |r| matches!(r, Row::Setting(i) if crate::config::schema::registry()[*i].path == path),
            )
        };
        let mut found = find(self.visible());
        if found.is_none()
            && let Some(def) = crate::config::schema::registry()
                .iter()
                .find(|d| d.path == path)
        {
            // A field to fill may sit behind its section's advanced toggle: open it.
            self.advanced_open.insert((def.tab, def.section));
            found = find(self.visible());
        }
        if let Some(at) = found {
            self.cursor = at;
        }
    }

    /// Structured editors manage one raw settings layer, not the merged effective table.
    /// Copying effective collections into a project would leak inherited credentials and pin
    /// unrelated global fields. Scalar overrides deliberately retain their effective seed.
    fn edit_seed(&self, def: &SettingDef) -> Value {
        if !matches!(def.kind, Kind::Toml) {
            return self.current(def);
        }
        let empty = || match def.default_value() {
            Some(Value::Array(_)) => Value::Array(Vec::new()),
            _ => Value::Table(Table::new()),
        };
        match self.pending.get(def.path) {
            Some(Pending::Set(value, scope)) if *scope == self.scope => value.clone(),
            Some(Pending::Unset(scope)) if *scope == self.scope => empty(),
            _ => crate::config::schema::get(self.layer_ref(self.scope), def.path)
                .cloned()
                .unwrap_or_else(empty),
        }
    }

    fn current(&self, def: &SettingDef) -> Value {
        match self.pending.get(def.path) {
            Some(Pending::Set(v, _)) => v.clone(),
            Some(Pending::Unset(scope)) => self
                .after_unset(def, *scope)
                .unwrap_or_else(|| Value::String(String::new())),
            None => crate::config::schema::get(&self.effective, def.path)
                .cloned()
                .or_else(|| def.default_value())
                .unwrap_or_else(|| Value::String(String::new())),
        }
    }

    /// `[ui]` as it will be once the in-flight changes land.
    fn preview_config(&self) -> UiConfig {
        let mut table = self.effective.clone();
        for (path, pending) in &self.pending {
            match pending {
                Pending::Set(v, _) => crate::config::schema::set(&mut table, path, v.clone()),
                Pending::Unset(scope) => {
                    match crate::config::schema::registry()
                        .iter()
                        .find(|d| d.path == path)
                        .and_then(|d| self.after_unset(d, *scope))
                    {
                        Some(v) => crate::config::schema::set(&mut table, path, v),
                        None => {
                            crate::config::schema::unset(&mut table, path);
                        }
                    }
                }
            }
        }
        if let Some((def, value)) = self.appearance_draft() {
            crate::config::schema::set(&mut table, def.path, value);
        }
        table
            .get("ui")
            .and_then(Value::as_table)
            .cloned()
            .and_then(|ui| Value::Table(ui).try_into().ok())
            .unwrap_or_default()
    }

    fn visible(&self) -> Vec<Row> {
        let defs = crate::config::schema::registry();
        if !self.search.is_empty() || self.changed_only {
            let mut hits: Vec<(i32, Row)> = Vec::new();
            for (i, def) in defs.iter().enumerate().filter(|(_, def)| {
                self.scope_shown(def)
                    && (!self.changed_only
                        || crate::config::schema::get(self.layer_ref(self.scope), def.path)
                            .is_some())
            }) {
                if let Some((score, _)) = self
                    .search
                    .is_empty()
                    .then_some((0, Vec::new()))
                    .or_else(|| search_match(&self.search, &haystack(def), def.description))
                    .or_else(|| {
                        // Exact descendant paths find their complete structured editor, including
                        // optional fields and operator-defined map entries absent from defaults.
                        (matches!(def.kind, Kind::Toml)
                            && self
                                .search
                                .to_lowercase()
                                .starts_with(&format!("{}.", def.path)))
                        .then_some((0, Vec::new()))
                    })
                {
                    // The whole query in the path or the label outranks any word-by-word hit;
                    // the exact path outranks everything.
                    let query = self.search.trim().to_lowercase();
                    let score = if query.eq_ignore_ascii_case(def.path) {
                        i32::MAX
                    } else if def.path.to_lowercase().contains(&query)
                        || def.label.to_lowercase().contains(&query)
                    {
                        score.saturating_add(100_000)
                    } else {
                        score
                    };
                    hits.push((score, Row::Setting(i)));
                }
            }
            for (i, (label, description, _)) in
                LINKS.iter().enumerate().filter(|_| !self.changed_only)
            {
                if let Some((score, _)) = search_match(
                    &self.search,
                    &format!("{label} {LINK_SECTION} {PROVIDERS_TAB}"),
                    description,
                ) {
                    hits.push((score, Row::Link(i)));
                }
            }
            hits.sort_by_key(|a| std::cmp::Reverse(a.0));
            return hits.into_iter().map(|(_, row)| row).collect();
        }
        match self.tabs.get(self.tab) {
            Some((tab, sections)) => {
                let mut rows = Vec::new();
                let launchers: &[usize] = match *tab {
                    "Connections" => &[0, 4],
                    "Model" => &[1, 5],
                    "Memory" => &[6, 7],
                    "Keys" => &[2],
                    "Learning" => &[3],
                    _ => &[],
                };
                rows.extend(launchers.iter().copied().map(Row::Link));
                for section in sections {
                    let in_section = |d: &SettingDef| {
                        d.tab == *tab
                            && d.section == *section
                            && self.memory_row_shown(d.path)
                            && self.scope_shown(d)
                    };
                    let basic = defs
                        .iter()
                        .enumerate()
                        .filter(|(_, d)| in_section(d) && !self.advanced(d.path))
                        .map(|(i, _)| Row::Setting(i));
                    let advanced: Vec<usize> = defs
                        .iter()
                        .enumerate()
                        .filter(|(_, d)| in_section(d) && self.advanced(d.path))
                        .map(|(i, _)| i)
                        .collect();
                    rows.extend(basic);
                    if advanced.is_empty() {
                        continue;
                    }
                    let open = self.advanced_open.contains(&(*tab, *section));
                    rows.push(Row::Advanced(tab, section, advanced.len()));
                    if open {
                        rows.extend(advanced.into_iter().map(Row::Setting));
                    }
                }
                // Common also lists the high-traffic settings that live elsewhere, in place: they
                // keep their home tab, search location and jump target, and edit the same value.
                if *tab == COMMON_TAB {
                    rows.extend(COMMON_MIRRORS.iter().filter_map(|path| {
                        defs.iter()
                            .position(|d| {
                                d.path == *path && d.tab != COMMON_TAB && self.scope_shown(d)
                            })
                            .map(Row::Setting)
                    }));
                }
                rows
            }
            None => (0..LINKS.len()).map(Row::Link).collect(),
        }
    }

    /// The heading a row sits under. In the Common tab every row is grouped as Common, mirrors
    /// included, so the tab reads as one short list rather than a heading per borrowed row.
    fn section_of(&self, row: Row) -> &'static str {
        match row {
            Row::Setting(_)
                if self.search.is_empty()
                    && !self.changed_only
                    && self
                        .tabs
                        .get(self.tab)
                        .is_some_and(|(t, _)| *t == COMMON_TAB) =>
            {
                COMMON_TAB
            }
            Row::Setting(i) => crate::config::schema::registry()[i].section,
            Row::Link(_) => LINK_SECTION,
            Row::Advanced(_, section, _) => section,
        }
    }

    // ---- input ----------------------------------------------------------------------------

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if self.bulk_review.is_some() {
            return self.bulk_handle(event, keys);
        }
        if self.panel_keys.handle_key(
            event,
            !matches!(self.mode, Mode::Browse) || !self.search.is_empty(),
        ) {
            return OverlayEvent::None;
        }
        if self.appearance_key(event, keys) {
            return OverlayEvent::None;
        }
        match event {
            Event::Key(key) if pressed(key) => match self.mode {
                Mode::Browse => self.browse_key(key, keys),
                Mode::Edit { .. } => self.edit_key(key, keys),
                Mode::Pick { .. } => self.pick_key(key, keys),
                Mode::Path { .. } => self.path_key(key, keys),
                Mode::List { .. } | Mode::Entries { .. } => self.structured_key(key, keys),
            },
            Event::Paste(pasted) => {
                let pasted = text::one_line(pasted);
                match &mut self.mode {
                    Mode::Edit { editor, def } => {
                        editor.insert(&pasted);
                        self.notes.remove(def.path);
                    }
                    Mode::Browse => {
                        self.search.push_str(&pasted);
                        self.cursor = 0;
                        self.scroll = 0;
                    }
                    Mode::Pick { filter, .. } => {
                        filter.insert(&pasted);
                    }
                    Mode::Path { browser, .. } => browser.paste(&pasted),
                    Mode::List { editor, .. } => editor.paste(&pasted),
                    Mode::Entries { editor, .. } => editor.paste(&pasted),
                }
                self.refit_pick()
            }
            _ => OverlayEvent::None,
        }
    }

    fn change_tab(&mut self, delta: isize) {
        if !self.search.is_empty() || self.changed_only {
            return;
        }
        let n = self.tabs.len() + 1;
        self.tab = (self.tab as isize + delta).rem_euclid(n as isize) as usize;
        self.cursor = 0;
        self.scroll = 0;
    }

    fn jump_section(&mut self, forward: bool) {
        let rows = self.visible();
        if rows.is_empty() {
            return;
        }
        let at = self.cursor.min(rows.len() - 1);
        let section = |i: usize| self.section_of(rows[i]);
        let group_start = |mut i: usize| {
            while i > 0 && section(i - 1) == section(i) {
                i -= 1;
            }
            i
        };
        self.cursor = if forward {
            (at + 1..rows.len())
                .find(|&i| section(i) != section(at))
                .unwrap_or(0)
        } else {
            let start = group_start(at);
            if at > start {
                start
            } else if start == 0 {
                group_start(rows.len() - 1)
            } else {
                group_start(start - 1)
            }
        };
    }

    fn current_row(&self) -> Option<Row> {
        let rows = self.visible();
        rows.get(self.cursor.min(rows.len().saturating_sub(1)))
            .copied()
    }

    /// Ctrl+L on a row whose program is missing: the first press shows the plan, the second starts the install.
    fn setup_key(&mut self) -> OverlayEvent {
        let Some(Row::Setting(i)) = self.current_row() else {
            return OverlayEvent::None;
        };
        let path = crate::config::schema::registry()[i].path;
        let Some(offer) = self.setup_offers.get(path) else {
            self.feedback = Some("Nothing to set up on this row".to_owned());
            return OverlayEvent::None;
        };
        if self.setup_armed.as_deref() == Some(offer.id.as_str()) {
            self.setup_armed = None;
            self.feedback = Some(format!("Installing {} in the background", offer.title));
            return OverlayEvent::SetupDep {
                id: offer.id.clone(),
            };
        }
        self.setup_armed = Some(offer.id.clone());
        self.feedback = Some(format!(
            "Review the plan below, then press Ctrl+L again to install {}",
            offer.title
        ));
        OverlayEvent::None
    }

    fn emit_set(&mut self, def: &'static SettingDef, value: Value) -> OverlayEvent {
        self.feedback = None;
        self.notes.remove(def.path);
        // One half of the embedding override alone is invalid: hold it until its partner is set,
        // then the controller saves both together (see `take_staged_pair`).
        if let Some(partner) = crate::config::schema::pair_partner(def.path)
            && value.as_str().is_some_and(|v| !v.trim().is_empty())
            && self
                .current_path(partner)
                .as_str()
                .is_none_or(|v| v.trim().is_empty())
        {
            self.pending
                .insert(def.path.to_owned(), Pending::Set(value.clone(), self.scope));
            self.staged_pair = Some((def.path, value));
            self.feedback = Some(format!(
                "{} saves together with {}: set it next",
                label_of(def.path),
                label_of(partner)
            ));
            self.move_to_path(partner);
            return OverlayEvent::None;
        }
        self.pending
            .insert(def.path.to_owned(), Pending::Set(value.clone(), self.scope));
        OverlayEvent::Set {
            path: def.path.to_owned(),
            value,
            scope: self.scope,
        }
    }

    fn activate(&mut self) -> OverlayEvent {
        let def: &'static SettingDef = match self.current_row() {
            Some(Row::Link(i)) => return OverlayEvent::Action(LINKS[i].2.to_owned()),
            Some(Row::Advanced(tab, section, _)) => {
                if !self.advanced_open.remove(&(tab, section)) {
                    self.advanced_open.insert((tab, section));
                }
                return OverlayEvent::None;
            }
            Some(Row::Setting(i)) => &crate::config::schema::registry()[i],
            None => return OverlayEvent::None,
        };
        let current = self.current(def);
        if !def.sensitive()
            && let Some(event) = self.open_source(def, &current)
        {
            return event;
        }
        match def.kind_for(&self.effective) {
            Kind::Bool if appearance::is_appearance(def) => {
                self.notes.remove(def.path);
                self.mode = Mode::Pick {
                    def,
                    choices: vec!["true".into(), "false".into()],
                    cursor: usize::from(!current.as_bool().unwrap_or(false)),
                    filter: Editor::default(),
                };
                OverlayEvent::None
            }
            Kind::Bool => {
                let next = !current.as_bool().unwrap_or(false);
                self.emit_set(def, Value::Boolean(next))
            }
            Kind::AutoBool => {
                let choices = vec!["auto".to_owned(), "true".to_owned(), "false".to_owned()];
                let cursor = choices
                    .iter()
                    .position(|choice| choice_value(Kind::AutoBool, choice) == current)
                    .unwrap_or(0);
                self.notes.remove(def.path);
                self.mode = Mode::Pick {
                    def,
                    choices,
                    cursor,
                    filter: Editor::default(),
                };
                OverlayEvent::None
            }
            Kind::Enum(_) | Kind::Choice(_) if def.path == EMBEDDING_MODEL_PATH => {
                self.open_embedding_picker(def)
            }
            Kind::Enum(options) | Kind::Choice(options) => {
                let mut choices: Vec<String> = options
                    .iter()
                    .map(|(value, _)| (*value).to_owned())
                    .collect();
                if matches!(def.kind, Kind::Choice(_))
                    && let Some(value) = current.as_str()
                    && !choices.iter().any(|choice| choice == value)
                {
                    choices.push(value.to_owned());
                }
                if choices.is_empty() {
                    return OverlayEvent::None;
                }
                // Returning to the backend picker mid-setup lands on the backend being set up.
                let wanted = match (def.path, &self.memory_intent) {
                    ("memory.backend", Some(intent)) => Some(intent.as_str()),
                    _ => current.as_str(),
                };
                let cursor = choices
                    .iter()
                    .position(|c| Some(c.as_str()) == wanted)
                    .unwrap_or(0);
                self.notes.remove(def.path);
                self.mode = Mode::Pick {
                    def,
                    choices,
                    cursor,
                    filter: Editor::default(),
                };
                OverlayEvent::None
            }
            Kind::Theme => {
                let mut choices = self.themes.clone();
                if def.path == "ui.theme" && !choices.iter().any(|t| t == "auto") {
                    choices.push("auto".to_owned());
                }
                if choices.is_empty() {
                    return OverlayEvent::None;
                }
                let cursor = choices
                    .iter()
                    .position(|c| Some(c.as_str()) == current.as_str())
                    .unwrap_or(0);
                self.notes.remove(def.path);
                self.mode = Mode::Pick {
                    def,
                    choices,
                    cursor,
                    filter: Editor::default(),
                };
                OverlayEvent::None
            }
            Kind::Table | Kind::Toml => {
                let seed = self.edit_seed(def);
                if !matches!(seed, Value::Table(_) | Value::Array(_)) {
                    return self.open_text_editor(def, &seed);
                }
                self.notes.remove(def.path);
                self.reveal_sensitive = false;
                self.mode = Mode::Entries {
                    def,
                    editor: Box::new(EntriesEditor::for_setting(seed, def.sensitive())),
                };
                OverlayEvent::None
            }
            Kind::Int { .. }
            | Kind::AutoInt { .. }
            | Kind::Float { .. }
            | Kind::AutoFloat { .. }
            | Kind::Text
            | Kind::StringList => {
                let seed = self.edit_seed(def);
                self.open_text_editor(def, &seed)
            }
        }
    }

    fn open_text_editor(&mut self, def: &'static SettingDef, seed: &Value) -> OverlayEvent {
        let mut editor = Editor::default();
        editor.set(&setting_edit_text(def, seed));
        self.reveal_sensitive = false;
        self.notes.remove(def.path);
        self.mode = Mode::Edit { def, editor };
        OverlayEvent::None
    }

    /// Open the chooser a setting's value source offers (see `SettingDef::source`); `None` keeps
    /// the kind's own editor.
    fn open_source(&mut self, def: &'static SettingDef, current: &Value) -> Option<OverlayEvent> {
        if matches!(def.path, "voice.tts.backend" | "voice.stt.backend") {
            self.download_armed = None;
            return self.open_offers(def, current);
        }
        let source = def.source()?;
        self.download_armed = None;
        let mode = match (source, def.kind_for(&self.effective)) {
            (Source::Path(kind), Kind::Text) => Mode::Path {
                def,
                browser: Box::new(PathBrowser::open(
                    kind,
                    current.as_str().unwrap_or_default(),
                    &self.base_dir,
                )),
            },
            (Source::Path(PathKind::Directory), Kind::StringList) => Mode::List {
                def,
                editor: Box::new(ListEditor::new(
                    current,
                    Vec::new(),
                    false,
                    AddBy::Folder,
                    &self.base_dir,
                )),
            },
            (Source::Values { values, ordered }, Kind::StringList) => Mode::List {
                def,
                editor: Box::new(ListEditor::new(
                    current,
                    values
                        .iter()
                        .map(|value| ((*value).to_owned(), (*value).to_owned()))
                        .collect(),
                    ordered,
                    AddBy::Text,
                    &self.base_dir,
                )),
            },
            // A list of models: check discovered ones, or add any other name.
            (Source::Model(_), Kind::StringList) => Mode::List {
                def,
                editor: Box::new(ListEditor::new(
                    current,
                    self.value_choices
                        .get(def.path)
                        .into_iter()
                        .flatten()
                        .map(|offer| (offer.value.clone(), offer.label.clone()))
                        .collect(),
                    false,
                    AddBy::Text,
                    &self.base_dir,
                )),
            },
            (Source::Model(_) | Source::Names(_), Kind::Text | Kind::Choice(_) | Kind::Enum(_)) => {
                return self.open_offers(def, current);
            }
            _ => return None,
        };
        self.notes.remove(def.path);
        self.mode = mode;
        Some(OverlayEvent::None)
    }

    /// A picker over the controller's offered values, the setting's own options and its current
    /// value, ending with a row that types any other value. `None` when nothing is offered.
    fn open_offers(&mut self, def: &'static SettingDef, current: &Value) -> Option<OverlayEvent> {
        let offers = self
            .value_choices
            .get(def.path)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut choices: Vec<String> = (0..offers.len())
            .map(|i| format!("{OFFER_PREFIX}{i}"))
            .collect();
        if let Kind::Choice(options) | Kind::Enum(options) = def.kind {
            choices.extend(
                options
                    .iter()
                    .filter(|(value, _)| !offers.iter().any(|offer| offer.value == *value))
                    .map(|(value, _)| (*value).to_owned()),
            );
        }
        let current = current.as_str().unwrap_or_default();
        let mut cursor = choices
            .iter()
            .position(|choice| self.choice_raw(def.path, choice) == current);
        if cursor.is_none() && !current.is_empty() {
            cursor = Some(choices.len());
            choices.push(current.to_owned());
        }
        if choices.is_empty() {
            return None;
        }
        if !matches!(def.kind, Kind::Enum(_)) {
            choices.push(TYPE_CHOICE.to_owned());
        }
        self.notes.remove(def.path);
        self.mode = Mode::Pick {
            def,
            choices,
            cursor: cursor.unwrap_or(0),
            filter: Editor::default(),
        };
        Some(OverlayEvent::None)
    }

    fn offer(&self, path: &str, choice: &str) -> Option<&ValueChoice> {
        let index = choice.strip_prefix(OFFER_PREFIX)?.parse::<usize>().ok()?;
        self.value_choices.get(path)?.get(index)
    }

    /// The value a picker row stands for.
    fn choice_raw<'a>(&'a self, path: &str, choice: &'a str) -> &'a str {
        self.offer(path, choice)
            .map_or(choice, |offer| offer.value.as_str())
    }

    /// Choosing an offered value: save it, assign the model role, or download then save. The
    /// first Enter on a model that is not downloaded only asks; the picker stays open.
    fn pick_offer(&mut self, def: &'static SettingDef, offer: ValueChoice) -> OverlayEvent {
        match offer.action {
            ChoiceAction::Unavailable { reason } => {
                self.download_armed = None;
                self.feedback = Some(reason);
                OverlayEvent::None
            }
            ChoiceAction::Install { .. }
                if self.download_armed.as_deref() != Some(offer.value.as_str()) =>
            {
                self.feedback = Some(format!(
                    "Install {}? Enter again: yes; Esc: cancel. Selection saves only after setup succeeds.",
                    offer.label
                ));
                self.download_armed = Some(offer.value);
                OverlayEvent::None
            }
            ChoiceAction::Install { .. } => {
                self.download_armed = None;
                self.mode = Mode::Browse;
                self.feedback = None;
                OverlayEvent::InstallVoice {
                    path: def.path.to_owned(),
                    value: offer.value,
                    scope: self.scope,
                }
            }
            ChoiceAction::Download
                if self.download_armed.as_deref() != Some(offer.value.as_str()) =>
            {
                self.feedback = Some(format!(
                    "{} is not downloaded yet. Press Enter again to download it and save {}; Esc cancels.",
                    offer.label, def.label
                ));
                self.download_armed = Some(offer.value);
                OverlayEvent::None
            }
            ChoiceAction::Download => {
                self.download_armed = None;
                self.mode = Mode::Browse;
                self.notes.remove(def.path);
                self.feedback = Some(format!(
                    "Downloading {}; {} is saved once the download finishes",
                    offer.label, def.label
                ));
                OverlayEvent::DownloadSetting {
                    path: def.path.to_owned(),
                    value: offer.value,
                    scope: self.scope,
                }
            }
            ChoiceAction::Assign { role, selection } => {
                self.download_armed = None;
                self.mode = Mode::Browse;
                self.feedback = None;
                OverlayEvent::AssignModel {
                    role,
                    selection,
                    scope: self.scope,
                }
            }
            ChoiceAction::Set => {
                self.download_armed = None;
                self.mode = Mode::Browse;
                let value = Value::String(offer.value);
                if self.current(def) == value {
                    OverlayEvent::None
                } else {
                    self.emit_set(def, value)
                }
            }
        }
    }

    fn reset_current(&mut self) -> OverlayEvent {
        let Some(Row::Setting(i)) = self.current_row() else {
            return OverlayEvent::None;
        };
        let def = &crate::config::schema::registry()[i];
        if crate::config::schema::get(self.layer_ref(self.scope), def.path).is_some() {
            self.notes.remove(def.path);
            self.pending
                .insert(def.path.to_owned(), Pending::Unset(self.scope));
            OverlayEvent::Unset {
                path: def.path.to_owned(),
                scope: self.scope,
            }
        } else {
            if crate::config::schema::get(self.layer_ref(other_scope(self.scope)), def.path)
                .is_some()
            {
                self.notes.insert(
                    def.path.to_owned(),
                    format!(
                        "Set in the {} scope; press Ctrl+S to switch scope, then Delete",
                        scope_name(other_scope(self.scope))
                    ),
                );
            }
            OverlayEvent::None
        }
    }

    // ---- drawing --------------------------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        if let Some(review) = &mut self.bulk_review {
            review.display_ready = false;
        }
        self.panel_keys.set_footer(self.full_footer(ctx.keys));
        if self.panel_keys.draw(
            frame,
            ctx,
            "Settings",
            "Type to search every setting; there is no separate search key.\nCtrl+S changes save scope · Ctrl+O filters saved overrides across categories · Delete resets the selected setting · Ctrl+L reviews available setup",
            true,
            false,
        ) {
            return;
        }
        let scope = match self.scope {
            Scope::Global => "All projects",
            Scope::Project => "This project",
        };
        // Narrow panels shed the key, then the label, so the scope never runs into the title.
        let count = self.pending_application_count();
        let pending = if count == 0 {
            String::new()
        } else {
            format!(" · {count} pending")
        };
        let full = format!(
            "Save to: {scope}{pending} {} Ctrl+S",
            ctx.theme.sym(Sym::Dot)
        );
        let short = format!("Save to: {scope}{pending}");
        let extra = super::palette::title_extra(
            ctx.theme,
            "Settings",
            frame.area().width,
            &[&full, &short, scope, ""],
        );
        panel(
            frame,
            ctx.theme,
            Section::Dial,
            "Settings",
            &extra,
            |w, h| self.compose(w, h, ctx),
        );
    }
}

#[cfg(test)]
mod tests;

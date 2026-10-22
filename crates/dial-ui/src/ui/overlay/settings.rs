//! The `/settings` panel: tab bar, section column, grouped value list, description, live status-line
//! preview and key hints. It never touches configuration: every change leaves as an
//! [`OverlayEvent`] and the displayed value only changes once the controller answers via
//! [`SettingsOverlay::settled`].
//!
//! This file also holds the small drawing helpers shared by the model and provider panels.
use super::{DrawCtx, OverlayEvent};
use crate::config::schema::{Kind, SettingDef};
use crate::config::settings_store::Scope;
use crate::llm::model_config::{ModelSelection, UiConfig};
use crate::ui::chrome::{self, Section, Status, status_chip};
use crate::ui::editor::Editor;
use crate::ui::keys::{Action, Keymap};
use crate::ui::text;
use crate::ui::theme::{Sym, Theme, Tok};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::collections::{BTreeMap, BTreeSet};
use toml::{Table, Value};
use unicode_segmentation::UnicodeSegmentation;

// ---------------------------------------------------------------------------------------------
// Shared helpers (also used by `models` and `providers`)
// ---------------------------------------------------------------------------------------------

pub(super) fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Global => "global",
        Scope::Project => "project",
    }
}

pub(super) fn other_scope(scope: Scope) -> Scope {
    match scope {
        Scope::Global => Scope::Project,
        Scope::Project => Scope::Global,
    }
}

pub(super) fn pressed(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
}

/// The character a key types into a text field (no Ctrl/Alt/Super chords, no controls).
pub(super) fn typed_char(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
                && !c.is_control() =>
        {
            Some(c)
        }
        _ => None,
    }
}

pub(super) fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| text::width(&s.content)).sum()
}

/// Truncate spans to `max` cells, never splitting a grapheme.
pub(super) fn fit(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut left = max;
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        if left == 0 {
            break;
        }
        let clipped = text::clip(&span.content, left);
        let cells = text::width(&clipped);
        if cells == 0 {
            continue;
        }
        left -= cells;
        out.push(Span::styled(clipped, span.style));
    }
    out
}

/// A row of exactly `width` cells whose unused part carries `fill` (selection backgrounds).
pub(super) fn filled(spans: Vec<Span<'static>>, width: usize, fill: Style) -> Line<'static> {
    let mut spans = fit(spans, width);
    let used = spans_width(&spans);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(spans).style(fill)
}

/// Spans for `text` with the characters at `positions` (char indexes) emphasized.
pub(super) fn highlighted(
    text: &str,
    positions: &[usize],
    base: Style,
    hl: Style,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut run_hl = false;
    for (i, c) in text.chars().enumerate() {
        let h = positions.contains(&i);
        if h != run_hl && !run.is_empty() {
            out.push(Span::styled(
                std::mem::take(&mut run),
                if run_hl { hl } else { base },
            ));
        }
        run_hl = h;
        run.push(c);
    }
    if !run.is_empty() {
        out.push(Span::styled(run, if run_hl { hl } else { base }));
    }
    out
}

pub(super) fn match_style(theme: &Theme, base: Style) -> Style {
    base.patch(theme.fg(Tok::Accent))
        .add_modifier(Modifier::BOLD)
}

/// Style of a selected row: the subtle selection fill with bold text.
pub(super) fn selected_fill(theme: &Theme) -> Style {
    chrome::selected_style(theme)
}

/// Style of a tab-bar cell; shared with every panel through [`chrome::tab_style`].
pub(super) fn tab_style(theme: &Theme, active: bool) -> Style {
    chrome::tab_style(theme, active)
}

/// One scrollbar cell for list row `row` of `rows` visible, `total` items, first visible `offset`.
pub(super) fn scroll_cell(
    theme: &Theme,
    row: usize,
    rows: usize,
    total: usize,
    offset: usize,
) -> Span<'static> {
    if total <= rows || rows == 0 {
        return Span::raw(" ");
    }
    let thumb = (rows * rows / total).max(1).min(rows);
    let start = if total > rows {
        offset * (rows - thumb) / (total - rows)
    } else {
        0
    };
    if row >= start && row < start + thumb {
        Span::styled(
            theme.sym(Sym::ScrollThumb).to_owned(),
            theme.fg(Tok::Accent),
        )
    } else {
        Span::styled(theme.sym(Sym::ScrollTrack).to_owned(), theme.fg(Tok::Dim))
    }
}

/// Footer hints: each segment's first word is the key (amber bold), the rest its label. When they do
/// not fit, segments are dropped from the one before the last (the last is always the "close" hint)
/// until they do.
pub(super) fn hint_line(theme: &Theme, segments: &[String], width: usize) -> Line<'static> {
    let mut keep: Vec<&str> = segments
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    while keep.len() > 1 && spans_width(&hint_key_spans(theme, &keep)) + 1 > width {
        keep.remove(keep.len() - 2);
    }
    let mut spans = vec![Span::raw(" ")];
    spans.extend(hint_key_spans(theme, &keep));
    Line::from(fit(spans, width))
}

fn hint_key_spans(theme: &Theme, segments: &[&str]) -> Vec<Span<'static>> {
    let pairs: Vec<(&str, &str)> = segments
        .iter()
        .map(|s| s.split_once(' ').unwrap_or((s, "")))
        .collect();
    chrome::key_hint_spans(theme, &pairs)
}

/// `Key` labels for "a/b" style hints; empty when the action is unbound.
pub(super) fn pair_label(keys: &Keymap, a: Action, b: Action) -> String {
    match (keys.label(a), keys.label(b)) {
        (x, y) if x.is_empty() => y,
        (x, y) if y.is_empty() => x,
        (x, y) => format!("{x}/{y}"),
    }
}

/// Which arrow pair a hint shows; ASCII themes get `<-/->` and `^v` instead of the Unicode glyphs.
#[derive(Clone, Copy)]
pub(super) enum Axis {
    Horizontal,
    Vertical,
}

/// The arrow pair for a hint, following the theme's ASCII mode.
pub(super) fn arrows(theme: &Theme, axis: Axis) -> String {
    match axis {
        Axis::Horizontal if theme.ascii() => "<-/->".to_owned(),
        Axis::Horizontal => "←/→".to_owned(),
        Axis::Vertical => format!("{}{}", theme.sym(Sym::ArrowUp), theme.sym(Sym::ArrowDown)),
    }
}

/// `"Esc close"`; empty when the action is unbound.
pub(super) fn hint(keys: &Keymap, action: Action, what: &str) -> String {
    let label = keys.label(action);
    if label.is_empty() {
        String::new()
    } else {
        format!("{label} {what}")
    }
}

/// Draw the full-screen panel: a rounded box whose top border carries the `◆ <name>` title and
/// `extra` at its right edge. `body` gets the inner size and returns at most that many rows. On a
/// terminal too small for the box the body fills the screen.
pub(super) fn panel(
    frame: &mut Frame,
    theme: &Theme,
    section: Section,
    name: &str,
    extra: &str,
    body: impl FnOnce(u16, u16) -> Vec<Line<'static>>,
) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < 5 || area.height < 5 {
        let mut lines = body(area.width, area.height);
        lines.truncate(usize::from(area.height));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let block = chrome::panel_block_titled(theme, section, name, extra);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = body(inner.width, inner.height);
    lines.truncate(usize::from(inner.height));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Cursor-aware rendering of a single-line editor inside `width` cells (the tail is kept visible).
pub(super) fn editor_spans(
    theme: &Theme,
    editor: &Editor,
    width: usize,
    mask: bool,
) -> Vec<Span<'static>> {
    let shown = |s: &str| {
        if mask {
            "•".repeat(s.graphemes(true).count())
        } else {
            s.to_owned()
        }
    };
    let before = shown(&editor.text[..editor.cursor.min(editor.text.len())]);
    let rest = &editor.text[editor.cursor.min(editor.text.len())..];
    let (at, after) = match rest.graphemes(true).next() {
        Some(g) => (shown(g), shown(&rest[g.len()..])),
        None => (" ".to_owned(), String::new()),
    };
    let at_w = text::width(&at).max(1);
    let budget = width.saturating_sub(at_w);
    // Keep the tail of `before`, then as much of `after` as still fits.
    let mut tail: Vec<&str> = Vec::new();
    let mut used = 0;
    for g in before.graphemes(true).rev() {
        let gw = text::width(g);
        if used + gw > budget {
            break;
        }
        used += gw;
        tail.push(g);
    }
    tail.reverse();
    let before: String = tail.concat();
    let after = text::clip(&after, budget.saturating_sub(used));
    let base = theme.fg(Tok::Text);
    fit(
        vec![
            Span::styled(before, base),
            Span::styled(at, base.add_modifier(Modifier::REVERSED)),
            Span::styled(after, base),
        ],
        width,
    )
}

/// A one-row tab strip of plain names two cells apart. It windows around `active` (drawn in the
/// active style when `highlight`), and an ellipsis on either side says more tabs are there
/// instead of clipping a name mid-word.
pub(super) fn tab_strip(
    theme: &Theme,
    names: &[&str],
    active: usize,
    highlight: bool,
    w: usize,
) -> Line<'static> {
    if names.is_empty() {
        return Line::default();
    }
    let gap = 2;
    let mark = theme.sym(Sym::Ellipsis);
    let mark_w = text::width(mark);
    let active = active.min(names.len() - 1);
    let span_w = |from: usize, to: usize| {
        names[from..=to]
            .iter()
            .map(|n| text::width(n))
            .sum::<usize>()
            + gap * (to - from)
    };
    let needs = |start: usize| {
        1 + if start > 0 { mark_w + 1 } else { 0 }
            + span_w(start, active)
            + if active + 1 < names.len() {
                gap + mark_w
            } else {
                0
            }
    };
    let mut start = 0;
    while start < active && needs(start) > w {
        start += 1;
    }
    let more = theme.fg(Tok::Dim);
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    if start > 0 {
        spans.push(Span::styled(format!("{mark} "), more));
        used += mark_w + 1;
    }
    for (i, name) in names.iter().enumerate().skip(start) {
        let sep = if i > start { gap } else { 0 };
        let reserve = if i + 1 < names.len() { gap + mark_w } else { 0 };
        let need = sep + text::width(name);
        if i > active && used + need + reserve > w {
            spans.push(Span::styled(format!("{}{mark}", " ".repeat(sep)), more));
            break;
        }
        spans.push(Span::raw(" ".repeat(sep)));
        spans.push(Span::styled(
            (*name).to_owned(),
            tab_style(theme, highlight && i == active),
        ));
        used += need;
    }
    Line::from(fit(spans, w))
}

#[cfg(test)]
pub(super) mod testkit;

// ---------------------------------------------------------------------------------------------
// Settings panel
// ---------------------------------------------------------------------------------------------

/// Choices a picker shows at once; longer lists get a filter line.
const PICK_ROWS: usize = 7;

/// Indexes of the `choices` the filter keeps, in order. Each space-separated term must appear in
/// the choice's label or raw value, ignoring case, so "auto" finds "Auto (let Dial decide)".
fn pick_matches(labels: &[String], choices: &[String], filter: &str) -> Vec<usize> {
    let terms: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
    (0..choices.len())
        .filter(|&n| {
            let haystack = format!("{} {}", labels[n], choices[n]).to_lowercase();
            terms.iter().all(|term| haystack.contains(term.as_str()))
        })
        .collect()
}

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

/// The other half of the OpenAI-compatible embedding override, which is valid only as a pair.
pub fn pair_partner(path: &str) -> Option<&'static str> {
    match path {
        EMBEDDING_URL_PATH => Some(EMBEDDING_MODEL_PATH),
        EMBEDDING_MODEL_PATH => Some(EMBEDDING_URL_PATH),
        _ => None,
    }
}

pub struct SettingsOverlay {
    effective: Table,
    project: Table,
    global: Table,
    scope: Scope,
    themes: Vec<String>,
    tabs: Vec<(&'static str, Vec<&'static str>)>,
    coding_only: bool,
    learning_only: bool,
    tab: usize,
    cursor: usize,
    scroll: usize,
    page: usize,
    search: String,
    mode: Mode,
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
}

/// Plain-language summary of a memory backend. `models` is the chord that opens the model selector
/// (its Roles view assigns the memory role), so the hint follows the user's keymap.
fn memory_summary(backend: &str, models: &str) -> String {
    match backend {
        "off" => {
            "Memory is off: nothing carries over between sessions and the memory tools are hidden."
                .to_owned()
        }
        "provenance" => {
            "Saves facts the agent learns, with history and expiry, found again by keyword. Needs no setup."
                .to_owned()
        }
        "local" => format!(
            "Summarises your past sessions into project memory, using the model on the Memory role ({models}, Roles)."
        ),
        "hindsight" => {
            "Connects to a Hindsight memory service you run. Needs its URL, and an API key variable for a remote server."
                .to_owned()
        }
        "sharpshooter" => format!(
            "Keeps project decision notes, written by the model on the Memory role ({models}, Roles)."
        ),
        "semantic" => format!(
            "Stores facts on this machine and recalls them by keyword, and by meaning once an embedding model is assigned (Embedding model below, or {models}, Roles). Facts are saved automatically only after you assign the memory role ({models}); that model receives turn text."
        ),
        _ => String::new(),
    }
}

/// Settings prefix holding one backend's own options.
fn memory_section(backend: &str) -> Option<&'static str> {
    match backend {
        "local" => Some("memory.local."),
        "hindsight" => Some("memory.hindsight."),
        "sharpshooter" => Some("memory.sharpshooter."),
        "semantic" => Some("memory.semantic."),
        _ => None,
    }
}

/// Whether `endpoint` names this machine, which may use plain HTTP and needs no API key.
/// Mirrors the loopback rule in `memory_backend::config::endpoint`.
fn is_loopback_url(endpoint: &str) -> bool {
    url::Url::parse(endpoint).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            host == "localhost"
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    })
}

/// Required fields of `backend` that are still empty or half set, as setting paths, in the order to
/// fill them. Mirrors `memory_backend::config::validate` and the runtime routes: Hindsight's URL and
/// key, Sharpshooter's model when the memory role is unassigned, and for semantic the missing half of
/// the URL/model override, or an embedding model when embeddings are `on` with no route.
fn memory_needs(
    backend: &str,
    get: impl Fn(&str) -> Value,
    has_role: impl Fn(&str) -> bool,
) -> Vec<&'static str> {
    let text = |path: &str| get(path).as_str().unwrap_or("").trim().to_owned();
    let mut needs = Vec::new();
    match backend {
        "hindsight" => {
            let endpoint = text("memory.hindsight.endpoint");
            if endpoint.is_empty() {
                needs.push("memory.hindsight.endpoint");
            } else if text("memory.hindsight.api_key_env").is_empty() && !is_loopback_url(&endpoint)
            {
                needs.push("memory.hindsight.api_key_env");
            }
        }
        "sharpshooter" => {
            if text("memory.sharpshooter.model").is_empty()
                && !has_role(&text("memory.sharpshooter.model_role"))
            {
                needs.push("memory.sharpshooter.model");
            }
        }
        "semantic" => {
            let (url, model) = (text(EMBEDDING_URL_PATH), text(EMBEDDING_MODEL_PATH));
            match (url.is_empty(), model.is_empty()) {
                (false, true) => needs.push(EMBEDDING_MODEL_PATH),
                (true, false) => needs.push(EMBEDDING_URL_PATH),
                (true, true)
                    if text("memory.semantic.embedding") == "on" && !has_role("embedding") =>
                {
                    needs.push(EMBEDDING_MODEL_PATH)
                }
                _ => {}
            }
        }
        _ => {}
    }
    needs
}

/// A model role the backend cannot work without and that only the Roles view can assign.
fn memory_missing_role(
    backend: &str,
    get: impl Fn(&str) -> Value,
    has_role: impl Fn(&str) -> bool,
) -> Option<String> {
    let fields: &[&str] = match backend {
        "local" => &[
            "memory.local.extraction_role",
            "memory.local.consolidation_role",
        ],
        _ => &[],
    };
    fields
        .iter()
        .filter_map(|path| get(path).as_str().map(str::to_owned))
        .find(|role| !has_role(role))
}

/// The display name of a `memory.backend` choice (`Hindsight service`).
fn label_for_choice(backend: &str) -> &'static str {
    crate::config::schema::registry()
        .iter()
        .find(|d| d.path == "memory.backend")
        .and_then(|d| match d.kind {
            Kind::Enum(options) => options.iter().find(|(v, _)| *v == backend).map(|(_, l)| *l),
            _ => None,
        })
        .unwrap_or("That backend")
}

fn label_of(path: &str) -> &'static str {
    crate::config::schema::registry()
        .iter()
        .find(|d| d.path == path)
        .map_or("", |d| d.label)
}

fn haystack(def: &SettingDef) -> String {
    format!("{} {} {} {}", def.label, def.section, def.tab, def.path)
}

/// Match words independently, keeping fuzzy matching on concise metadata rather than long prose.
/// Description-only hits still match, but do not invent highlights in the setting's label.
fn search_match(query: &str, metadata: &str, description: &str) -> Option<(i32, Vec<usize>)> {
    let description = description.to_lowercase();
    let mut score = 0;
    let mut positions = Vec::new();
    for word in query.split_whitespace() {
        if let Some((rank, matched)) = text::fuzzy(word, metadata) {
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

/// Pickers keep string identifiers, but automatic switches persist actual TOML booleans.
fn choice_value(kind: Kind, choice: &str) -> Value {
    match (kind, choice) {
        (Kind::AutoBool, "true") => Value::Boolean(true),
        (Kind::AutoBool, "false") => Value::Boolean(false),
        _ => Value::String(choice.to_owned()),
    }
}

fn fmt_float(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

fn value_text(def: &SettingDef, value: &Value) -> String {
    if def.path == "harness.max_steps" && value.as_integer() == Some(0) {
        return "0 (unlimited)".into();
    }
    let raw = match (&def.kind, value) {
        (Kind::AutoInt { .. } | Kind::AutoFloat { .. }, Value::String(s)) if s == "auto" => {
            "Auto".into()
        }
        (Kind::AutoBool, Value::String(s)) if s == "auto" => "auto".to_owned(),
        (Kind::Bool | Kind::AutoBool, Value::Boolean(b)) => {
            if *b { "on" } else { "off" }.to_owned()
        }
        (Kind::Enum(options) | Kind::Choice(options), Value::String(s)) => options
            .iter()
            .find(|(v, _)| v == s)
            .map_or_else(|| s.clone(), |(_, label)| (*label).to_owned()),
        (_, Value::String(s)) if s.is_empty() => "(empty)".to_owned(),
        (_, Value::String(s)) => s.clone(),
        (_, Value::Integer(n)) => n.to_string(),
        (_, Value::Float(f)) => fmt_float(*f),
        (_, Value::Boolean(b)) => b.to_string(),
        (_, Value::Array(items)) if items.is_empty() => "(none)".to_owned(),
        (_, Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        (_, other) => other.to_string(),
    };
    text::one_line(&raw)
}

fn edit_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Integer(n) => n.to_string(),
        Value::Float(f) => fmt_float(*f),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn input_hint(kind: Kind) -> String {
    match kind {
        Kind::AutoInt { min, max, .. } => format!("Auto or a whole number from {min} to {max}."),
        Kind::AutoFloat { min, max, .. } => format!(
            "Auto or a number from {} to {}.",
            fmt_float(min),
            fmt_float(max)
        ),
        Kind::Int { min, max, .. } => format!("Whole number from {min} to {max}."),
        Kind::Float { min, max, .. } => {
            format!("Number from {} to {}.", fmt_float(min), fmt_float(max))
        }
        Kind::StringList => "Comma-separated list.".to_owned(),
        Kind::Table => {
            "TOML inline table, for example { \"provider/model\" = \"80%\" }.".to_owned()
        }
        _ => "Text.".to_owned(),
    }
}

/// Parse and range-check typed input for `kind`.
fn parse_input(kind: Kind, input: &str) -> Result<Value, String> {
    let input = input.trim();
    match kind {
        Kind::AutoInt { .. } | Kind::AutoFloat { .. } if input.eq_ignore_ascii_case("auto") => {
            Ok(Value::String("auto".into()))
        }
        Kind::Int { min, max, .. } | Kind::AutoInt { min, max, .. } => {
            let n: i64 = input
                .parse()
                .map_err(|_| format!("Enter a whole number between {min} and {max}"))?;
            if (min..=max).contains(&n) {
                Ok(Value::Integer(n))
            } else {
                Err(format!("Must be between {min} and {max}"))
            }
        }
        Kind::Float { min, max, .. } | Kind::AutoFloat { min, max, .. } => {
            let f: f64 = input
                .parse()
                .ok()
                .filter(|f: &f64| f.is_finite())
                .ok_or_else(|| {
                    format!(
                        "Enter a number between {} and {}",
                        fmt_float(min),
                        fmt_float(max)
                    )
                })?;
            if f >= min && f <= max {
                Ok(Value::Float(f))
            } else {
                Err(format!(
                    "Must be between {} and {}",
                    fmt_float(min),
                    fmt_float(max)
                ))
            }
        }
        Kind::StringList => Ok(Value::Array(
            input
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        )),
        Kind::Table => {
            if input.len() > 16 * 1024 {
                return Err("Settings table exceeds 16 KiB".into());
            }
            let mut table: Table = toml::from_str(&format!("value = {input}"))
                .map_err(|_| "Enter a valid TOML inline table".to_owned())?;
            let value = table.remove("value").ok_or("Enter a TOML inline table")?;
            if !value.is_table() || !table.is_empty() {
                return Err("Enter one TOML inline table".into());
            }
            Ok(value)
        }
        _ => Ok(Value::String(input.to_owned())),
    }
}

/// Step a numeric buffer up (`dir = 1`) or down.
fn adjust(kind: Kind, buffer: &str, dir: i64) -> Option<String> {
    match kind {
        Kind::AutoInt { min, .. } if buffer.trim().eq_ignore_ascii_case("auto") => {
            Some(if dir > 0 {
                min.to_string()
            } else {
                "auto".into()
            })
        }
        Kind::AutoInt { min, .. } if dir < 0 && buffer.trim().parse::<i64>().ok() == Some(min) => {
            Some("auto".into())
        }
        Kind::Int { min, max, step } | Kind::AutoInt { min, max, step } => {
            let n = buffer.trim().parse::<i64>().unwrap_or(min);
            Some(
                n.saturating_add(step.saturating_mul(dir))
                    .clamp(min, max)
                    .to_string(),
            )
        }
        Kind::AutoFloat { min, .. } if buffer.trim().eq_ignore_ascii_case("auto") => {
            Some(if dir > 0 {
                fmt_float(min)
            } else {
                "auto".into()
            })
        }
        Kind::AutoFloat { min, .. }
            if dir < 0
                && buffer
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|value| value <= min) =>
        {
            Some("auto".into())
        }
        Kind::Float { min, max, step } | Kind::AutoFloat { min, max, step } => {
            let f = buffer
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .unwrap_or(min);
            Some(fmt_float(
                ((f + step * dir as f64).clamp(min, max) * 1e6).round() / 1e6,
            ))
        }
        _ => None,
    }
}

impl SettingsOverlay {
    pub fn new(
        values: Table,
        project_values: Table,
        global_values: Table,
        scope: Scope,
        themes: Vec<String>,
    ) -> Self {
        SettingsOverlay {
            effective: values,
            project: project_values,
            global: global_values,
            scope,
            themes,
            tabs: crate::config::schema::tabs(),
            coding_only: false,
            learning_only: false,
            tab: 0,
            cursor: 0,
            scroll: 0,
            page: 8,
            search: String::new(),
            mode: Mode::Browse,
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
            feedback: None,
            load_error: None,
        }
    }

    pub fn coding_only(mut self) -> Self {
        self.coding_only = true;
        self.filter_tabs();
        self
    }
    pub fn learning_only(mut self) -> Self {
        self.learning_only = true;
        self.filter_tabs();
        self
    }

    /// Shared defaults for future model selections, rather than a current-model override.
    pub fn context_defaults(mut self) -> Self {
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
        def.tab == "Improvement"
            || def.path.starts_with("laya.")
            || def.path.starts_with("memory.autolearn")
            || def.path.starts_with("memory.learn_")
    }
    fn scope_shown(&self, def: &SettingDef) -> bool {
        // The one learning switch that changes coding sessions belongs in both views.
        if def.path == "improve.teach" {
            return true;
        }
        let learning = Self::learning_setting(def);
        (!self.coding_only || !learning) && (!self.learning_only || learning)
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
            || (EMBEDDING_OVERRIDE_ROWS.contains(&path)
                && self
                    .current_path(EMBEDDING_URL_PATH)
                    .as_str()
                    .is_none_or(|url| url.trim().is_empty()))
    }

    /// The Embedding model row's value: the override when set, otherwise the embedding role.
    fn embedding_row_text(&self, value: &Value) -> String {
        let model = value.as_str().unwrap_or("").trim();
        if !model.is_empty() {
            return format!("{model} · URL override");
        }
        match self.embedding_role() {
            Some(name) => format!("{name} · embedding role"),
            None => "not set · Enter to choose".to_owned(),
        }
    }

    /// `provider/model` of the assigned embedding role.
    fn embedding_role(&self) -> Option<String> {
        let role = crate::config::schema::get(&self.effective, "model_roles.embedding")?;
        let field = |key: &str| role.get(key).and_then(Value::as_str).unwrap_or("");
        Some(format!("{}/{}", field("provider"), field("model")))
    }

    /// Enter on the Embedding model row: discovered models for the embedding role, curated models to
    /// set up, then the advanced override.
    fn open_embedding_picker(&mut self, def: &'static SettingDef) -> OverlayEvent {
        let assigned = self.embedding_role();
        let mut choices: Vec<String> = (0..self.embedding_models.len())
            .map(|i| format!("role:{i}"))
            .collect();
        let discovered = |name: &str| {
            self.embedding_models.iter().any(|model| {
                model.selection.provider == "ollama"
                    && model.selection.model.trim_end_matches(":latest") == name
            })
        };
        choices.extend(
            crate::setup::embeddings::CATALOG
                .iter()
                .filter(|model| !discovered(model.name))
                .map(|model| format!("setup:{}", model.name)),
        );
        choices.push("override".to_owned());
        let has_override = self
            .current(def)
            .as_str()
            .is_some_and(|model| !model.trim().is_empty());
        if has_override {
            choices.push("clear".to_owned());
        }
        // A URL override waiting for its model lands on typing the model name.
        let url_set = self
            .current_path(EMBEDDING_URL_PATH)
            .as_str()
            .is_some_and(|url| !url.trim().is_empty());
        let assigned_choice = |choice: &String| {
            choice
                .strip_prefix("role:")
                .and_then(|i| i.parse::<usize>().ok())
                .and_then(|i| self.embedding_models.get(i))
                .is_some_and(|model| {
                    assigned.as_deref()
                        == Some(&format!(
                            "{}/{}",
                            model.selection.provider, model.selection.model
                        ))
                })
        };
        let cursor = choices
            .iter()
            .position(|choice| {
                if url_set {
                    choice == "override"
                } else {
                    assigned_choice(choice)
                }
            })
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

    fn embedding_choice_label(&self, choice: &str) -> String {
        if let Some(model) = choice
            .strip_prefix("role:")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.embedding_models.get(i))
        {
            let name = format!("{}/{}", model.selection.provider, model.label);
            return if model.classified {
                format!("{name} · embedding role")
            } else {
                format!(
                    "{name} · {}",
                    crate::ui::overlay::models::UNCLASSIFIED_LABEL
                )
            };
        }
        if let Some(name) = choice.strip_prefix("setup:") {
            let size = crate::setup::embeddings::find(name)
                .map(|model| format!(", {} MB", model.size_mb))
                .unwrap_or_default();
            return format!("{name} · set up with Ollama{size}, then Ctrl+L");
        }
        match choice {
            "override" => {
                "Type a model name (advanced override; needs Embedding service URL)".into()
            }
            "clear" => "Clear the URL/model override (use the embedding role)".into(),
            other => other.to_owned(),
        }
    }

    /// A choice on the Embedding model row: assign the role, arm a curated setup for Ctrl+L, edit
    /// the override, or clear it.
    fn embedding_pick(&mut self, def: &'static SettingDef, choice: &str) -> OverlayEvent {
        if let Some(model) = choice
            .strip_prefix("role:")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.embedding_models.get(i))
        {
            let overridden = self
                .current(def)
                .as_str()
                .is_some_and(|model| !model.trim().is_empty());
            self.feedback = overridden.then(|| {
                "The URL/model override still wins over the embedding role; clear it to use the role"
                    .to_owned()
            });
            return OverlayEvent::AssignModel {
                role: "embedding".into(),
                selection: model.selection.clone(),
                scope: self.scope,
            };
        }
        if let Some(name) = choice.strip_prefix("setup:") {
            match self.embedding_setups.get(name).cloned() {
                Some(offer) => {
                    self.setup_armed = Some(offer.id.clone());
                    self.feedback = Some(format!(
                        "Review the plan below, then press Ctrl+L to set up {}",
                        offer.title
                    ));
                    self.setup_offers.insert(def.path.to_owned(), offer);
                }
                None => {
                    self.feedback = Some(format!("Run /setup embeddings:{name} to set it up"));
                }
            }
            return OverlayEvent::None;
        }
        match choice {
            "override" => {
                let mut editor = Editor::default();
                editor.set(&edit_text(&self.current(def)));
                self.mode = Mode::Edit { def, editor };
                OverlayEvent::None
            }
            "clear" => {
                self.notes.remove(def.path);
                self.pending
                    .insert(def.path.to_owned(), Pending::Unset(self.scope));
                OverlayEvent::Unset {
                    path: def.path.to_owned(),
                    scope: self.scope,
                }
            }
            _ => OverlayEvent::None,
        }
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
        crate::config::schema::get(
            self.layer_ref(super::settings::other_scope(scope)),
            def.path,
        )
        .cloned()
        .or_else(|| def.default_value())
    }

    fn current_path(&self, path: &str) -> Value {
        crate::config::schema::registry()
            .iter()
            .find(|d| d.path == path)
            .map_or(Value::String(String::new()), |d| self.current(d))
    }

    fn memory_backend(&self) -> String {
        self.current_path("memory.backend")
            .as_str()
            .unwrap_or("provenance")
            .to_owned()
    }

    /// Memory rows that do nothing for the selected backend stay out of the way.
    fn memory_row_shown(&self, path: &str) -> bool {
        if !path.starts_with("memory.") || path == "memory.backend" {
            return true;
        }
        let backend = self.memory_backend();
        if backend == "off" {
            return false;
        }
        let own = [Some(backend.as_str()), self.memory_intent.as_deref()];
        ["local", "hindsight", "sharpshooter", "semantic"]
            .iter()
            .filter_map(|b| memory_section(b).map(|prefix| (b, prefix)))
            .all(|(b, prefix)| !path.starts_with(prefix) || own.contains(&Some(*b)))
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
        table
            .get("ui")
            .and_then(Value::as_table)
            .cloned()
            .and_then(|ui| Value::Table(ui).try_into().ok())
            .unwrap_or_default()
    }

    fn visible(&self) -> Vec<Row> {
        let defs = crate::config::schema::registry();
        if !self.search.is_empty() {
            let mut hits: Vec<(i32, Row)> = Vec::new();
            for (i, def) in defs
                .iter()
                .enumerate()
                .filter(|(_, def)| self.scope_shown(def))
            {
                if let Some((score, _)) =
                    search_match(&self.search, &haystack(def), def.description)
                {
                    hits.push((score, Row::Setting(i)));
                }
            }
            for (i, (label, description, _)) in LINKS.iter().enumerate() {
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
                rows
            }
            None => (0..LINKS.len()).map(Row::Link).collect(),
        }
    }

    fn section_of(row: Row) -> &'static str {
        match row {
            Row::Setting(i) => crate::config::schema::registry()[i].section,
            Row::Link(_) => LINK_SECTION,
            Row::Advanced(_, section, _) => section,
        }
    }

    // ---- input ----------------------------------------------------------------------------

    pub fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        match event {
            Event::Key(key) if pressed(key) => match self.mode {
                Mode::Browse => self.browse_key(key, keys),
                Mode::Edit { .. } => self.edit_key(key, keys),
                Mode::Pick { .. } => self.pick_key(key, keys),
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
                }
                self.refit_pick()
            }
            _ => OverlayEvent::None,
        }
    }

    fn browse_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        self.feedback = None;
        if is_ctrl(key, 's') {
            self.scope = other_scope(self.scope);
            // The title's scope label is shed on narrow panels, so say what changed here too.
            self.feedback = Some(
                match self.scope {
                    Scope::Global => "Changes now save for all projects",
                    Scope::Project => "Changes now save for this project only",
                }
                .to_owned(),
            );
            return OverlayEvent::None;
        }
        if is_ctrl(key, 'l') && !keys.matches(Action::TabNext, key) {
            return self.setup_key();
        }
        if keys.matches(Action::SelectCancel, key) {
            if self.search.is_empty() {
                return OverlayEvent::Close;
            }
            self.search.clear();
            self.cursor = 0;
            self.scroll = 0;
            return OverlayEvent::None;
        }
        let count = self.visible().len();
        let last = count.saturating_sub(1);
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
        } else if key.code == KeyCode::Home {
            self.cursor = 0;
        } else if key.code == KeyCode::End {
            self.cursor = last;
        } else if keys.matches(Action::SelectConfirm, key) {
            return self.activate();
        } else if keys.matches(Action::TabNext, key) {
            self.change_tab(1);
        } else if keys.matches(Action::TabPrev, key) {
            self.change_tab(-1);
        } else if keys.matches(Action::SectionNext, key) {
            self.jump_section(true);
        } else if keys.matches(Action::SectionPrev, key) {
            self.jump_section(false);
        } else if key.code == KeyCode::Delete {
            return self.reset_current();
        } else if key.code == KeyCode::Backspace {
            if self.search.pop().is_some() {
                self.cursor = 0;
                self.scroll = 0;
            }
        } else if let Some(c) = typed_char(key) {
            if self.search.is_empty() && c == ' ' {
                return self.activate();
            }
            self.search.push(c);
            self.cursor = 0;
            self.scroll = 0;
        }
        OverlayEvent::None
    }

    fn change_tab(&mut self, delta: isize) {
        if !self.search.is_empty() {
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
        let section = |i: usize| Self::section_of(rows[i]);
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
        if let Some(partner) = pair_partner(def.path)
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
        match def.kind_for(&self.effective) {
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
            Kind::Int { .. }
            | Kind::AutoInt { .. }
            | Kind::Float { .. }
            | Kind::AutoFloat { .. }
            | Kind::Text
            | Kind::StringList
            | Kind::Table => {
                let mut editor = Editor::default();
                editor.set(&edit_text(&current));
                self.notes.remove(def.path);
                self.mode = Mode::Edit { def, editor };
                OverlayEvent::None
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

    fn edit_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Edit { def, editor } = &mut self.mode else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        if keys.matches(Action::SelectCancel, key) {
            self.notes.remove(def.path);
            self.mode = Mode::Browse;
        } else if keys.matches(Action::SelectConfirm, key) {
            match parse_input(def.kind_for(&self.effective), &editor.text) {
                Ok(value) => {
                    self.mode = Mode::Browse;
                    return self.emit_set(def, value);
                }
                Err(message) => {
                    self.notes.insert(def.path.to_owned(), message);
                }
            }
        } else if keys.matches(Action::SelectUp, key) || keys.matches(Action::SelectDown, key) {
            let dir = if keys.matches(Action::SelectUp, key) {
                1
            } else {
                -1
            };
            if let Some(next) = adjust(def.kind_for(&self.effective), &editor.text, dir) {
                editor.set(&next);
                self.notes.remove(def.path);
            }
        } else if editor.key(*key) {
            self.notes.remove(def.path);
        }
        OverlayEvent::None
    }

    fn pick_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        let Mode::Pick { def, .. } = &self.mode else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        let labels: Vec<String> = match &self.mode {
            Mode::Pick { choices, .. } => choices
                .iter()
                .map(|choice| self.choice_label(def, choice))
                .collect(),
            _ => return OverlayEvent::None,
        };
        let is_theme = matches!(def.kind_for(&self.effective), Kind::Theme);
        let page = self.page.max(1);
        let Mode::Pick {
            choices,
            cursor,
            filter,
            ..
        } = &mut self.mode
        else {
            return OverlayEvent::None;
        };
        let at = *cursor;
        let visible = pick_matches(&labels, choices, &filter.text);
        if keys.matches(Action::SelectCancel, key) {
            // Esc first clears the filter; only a bare Esc leaves the picker.
            if !filter.text.is_empty() {
                filter.set("");
                return OverlayEvent::None;
            }
            self.mode = Mode::Browse;
            if def.path == "memory.backend" {
                self.memory_intent = None;
            }
            return if is_theme {
                OverlayEvent::PreviewTheme(None)
            } else {
                OverlayEvent::None
            };
        }
        if keys.matches(Action::SelectConfirm, key) {
            // A filter that matches nothing has nothing to save; Esc clears it.
            if !visible.contains(&at) {
                return OverlayEvent::None;
            }
            let chosen = choices[at].clone();
            let value = choice_value(def.kind_for(&self.effective), &chosen);
            let unchanged = self.current(def) == value;
            self.mode = Mode::Browse;
            if def.path == EMBEDDING_MODEL_PATH {
                return self.embedding_pick(def, &chosen);
            }
            if def.path == "memory.backend" {
                if let Some(role) =
                    memory_missing_role(&chosen, |p| self.current_path(p), |r| self.has_role(r))
                {
                    // Only the Roles view assigns a model role; switching now would leave memory paused.
                    self.memory_intent = None;
                    self.feedback = Some(format!(
                        "{} needs the {role} role: assign it in /roles (Alt+M), then choose it here.",
                        label_for_choice(&chosen)
                    ));
                    return OverlayEvent::None;
                }
                let needs = memory_needs(&chosen, |p| self.current_path(p), |r| self.has_role(r));
                if let Some(first) = needs.first() {
                    // Saving the backend now would fail validation or leave it inactive: collect
                    // its fields first.
                    let labels: Vec<_> = needs.iter().map(|p| label_of(p)).collect();
                    let alternative = if chosen == "sharpshooter" {
                        " (or assign the memory role in /roles, Alt+M)"
                    } else {
                        ""
                    };
                    self.memory_intent = Some(chosen.clone());
                    self.feedback = Some(format!(
                        "To switch, first fill: {}{alternative}. You'll come back here when they're set.",
                        labels.join(", ")
                    ));
                    self.move_to_path(first);
                    return OverlayEvent::None;
                }
                self.memory_intent = None;
            }
            return if unchanged {
                if is_theme {
                    OverlayEvent::PreviewTheme(None)
                } else {
                    OverlayEvent::None
                }
            } else {
                self.emit_set(def, value)
            };
        }
        let pos = visible.iter().position(|&n| n == at).unwrap_or(0);
        let last = visible.len().saturating_sub(1);
        let next = if keys.matches(Action::SelectUp, key) {
            Some(if pos == 0 { last } else { pos - 1 })
        } else if keys.matches(Action::SelectDown, key) {
            Some(if pos >= last { 0 } else { pos + 1 })
        } else if keys.matches(Action::SelectPageUp, key) {
            Some(pos.saturating_sub(page))
        } else if keys.matches(Action::SelectPageDown, key) {
            Some((pos + page).min(last))
        } else if filter.text.is_empty() && key.code == KeyCode::Home {
            Some(0)
        } else if filter.text.is_empty() && key.code == KeyCode::End {
            Some(last)
        } else {
            None
        };
        if let Some(next) = next {
            let Some(&n) = visible.get(next) else {
                return OverlayEvent::None;
            };
            *cursor = n;
            return if is_theme {
                OverlayEvent::PreviewTheme(Some(choices[n].clone()))
            } else {
                OverlayEvent::None
            };
        }
        // Any other editing key goes to the filter; only a change to its text re-fits the selection.
        let before = filter.text.clone();
        if !filter.key_with(*key, keys) || filter.text == before {
            return OverlayEvent::None;
        }
        self.refit_pick()
    }

    /// After the filter text changed, keep the selection on a choice it still shows. The chosen
    /// value is untouched until Enter; a theme picker previews whichever choice is now selected.
    fn refit_pick(&mut self) -> OverlayEvent {
        let Mode::Pick { def, choices, .. } = &self.mode else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        let labels: Vec<String> = choices
            .iter()
            .map(|choice| self.choice_label(def, choice))
            .collect();
        let is_theme = matches!(def.kind_for(&self.effective), Kind::Theme);
        let Mode::Pick {
            choices,
            cursor,
            filter,
            ..
        } = &mut self.mode
        else {
            return OverlayEvent::None;
        };
        let visible = pick_matches(&labels, choices, &filter.text);
        if visible.contains(&*cursor) {
            return OverlayEvent::None;
        }
        let Some(&first) = visible.first() else {
            return OverlayEvent::None;
        };
        *cursor = first;
        if is_theme {
            OverlayEvent::PreviewTheme(Some(choices[first].clone()))
        } else {
            OverlayEvent::None
        }
    }

    /// What a picker row shows for a raw choice value. The saved value is always the raw one.
    fn choice_label(&self, def: &SettingDef, choice: &str) -> String {
        match (def.kind_for(&self.effective), choice) {
            (Kind::AutoBool, "auto") => "Auto (let Dial decide)".to_owned(),
            (Kind::AutoBool, "true") => "On (enabled)".to_owned(),
            (Kind::AutoBool, "false") => "Off (disabled)".to_owned(),
            _ if def.path == EMBEDDING_MODEL_PATH => self.embedding_choice_label(choice),
            _ => value_text(def, &Value::String(choice.to_owned())),
        }
    }

    /// Indexes of the choices the picker filter keeps, in order.
    fn pick_visible(&self, def: &SettingDef, choices: &[String], filter: &str) -> Vec<usize> {
        let labels: Vec<String> = choices
            .iter()
            .map(|choice| self.choice_label(def, choice))
            .collect();
        pick_matches(&labels, choices, filter)
    }

    // ---- drawing --------------------------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let scope = match self.scope {
            Scope::Global => "All projects",
            Scope::Project => "This project",
        };
        // Narrow panels shed the key, then the label, so the scope never runs into the title.
        let full = format!("Save to: {scope} {} Ctrl+S", ctx.theme.sym(Sym::Dot));
        let short = format!("Save to: {scope}");
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

    fn compose(&mut self, w: u16, h: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(w), usize::from(h));
        let theme = ctx.theme;
        let rows = self.visible();
        self.cursor = self.cursor.min(rows.len().saturating_sub(1));
        let cursor_row = rows.get(self.cursor).copied();
        let appearance = matches!(cursor_row, Some(Row::Setting(i)) if crate::config::schema::registry()[i].tab == "Appearance");
        // The preview row appears only when the status line has something to show for it.
        let preview_label = " Preview: ";
        let preview_room = w.saturating_sub(preview_label.len());
        let preview = (appearance && preview_room > 0)
            .then(|| {
                (ctx.preview_status)(
                    &self.preview_config(),
                    preview_room.min(usize::from(u16::MAX)) as u16,
                )
            })
            .filter(|line| line.width() > 0);
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
        // Keep navigation and search visible before optional details on short terminals.
        let (show_footer, show_search, show_tabs, show_desc, show_desc2) =
            (grant(), grant(), grant(), grant(), grant());
        let show_preview = preview.is_some() && grant();
        let body_rows = h - [
            show_tabs,
            show_footer,
            show_preview,
            show_desc,
            show_search,
            show_desc2,
        ]
        .iter()
        .filter(|b| **b)
        .count();
        self.page = body_rows.saturating_sub(1).max(1);

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(h);
        if show_tabs {
            lines.push(self.tab_bar(theme, w));
        }
        if show_search {
            lines.push(self.search_line(theme, w));
        }

        // Left column of sections.
        let longest = self.tabs.get(self.tab).map_or(0, |(_, s)| {
            s.iter().map(|s| text::width(s)).max().unwrap_or(0)
        });
        let left_w = if w >= 64 {
            (longest + 4).max(w / 4).min(28).min(w / 3)
        } else {
            0
        };
        let list_w_total = w - if left_w > 0 { left_w + 1 } else { 0 };

        // Right list items.
        let searching = !self.search.is_empty();
        let mut items: Vec<Item> = Vec::new();
        let mut cursor_item = 0;
        let mut last_section = "";
        for (i, row) in rows.iter().enumerate() {
            let section = Self::section_of(*row);
            if !searching && section != last_section {
                items.push(Item::Header(section));
                last_section = section;
            }
            if i == self.cursor {
                cursor_item = items.len();
            }
            items.push(Item::Row(i));
            if let Row::Setting(s) = row {
                let def = &crate::config::schema::registry()[*s];
                if let Mode::Pick {
                    def: picking,
                    choices,
                    cursor,
                    filter,
                } = &self.mode
                    && picking.path == def.path
                    && i == self.cursor
                {
                    let labels: Vec<String> = choices
                        .iter()
                        .map(|choice| self.choice_label(def, choice))
                        .collect();
                    let visible = pick_matches(&labels, choices, &filter.text);
                    // Show every match the panel has room for (beside the setting row, its
                    // section header and the filter line); short panels keep the usual window.
                    let pick_rows = body_rows.saturating_sub(3).max(PICK_ROWS);
                    if choices.len() > PICK_ROWS || !filter.text.is_empty() {
                        items.push(Item::Filter {
                            editor: filter.clone(),
                            shown: visible.len(),
                            total: choices.len(),
                        });
                    }
                    if visible.is_empty() {
                        items.push(Item::Hint(format!(
                            "No option matches \"{}\". Esc clears the filter.",
                            text::one_line(&filter.text)
                        )));
                    }
                    let at = visible.iter().position(|&n| n == *cursor).unwrap_or(0);
                    let first = at
                        .saturating_sub(pick_rows / 2)
                        .min(visible.len().saturating_sub(pick_rows));
                    for &n in visible.iter().skip(first).take(pick_rows) {
                        if n == *cursor {
                            cursor_item = items.len();
                        }
                        items.push(Item::Choice {
                            name: labels[n].clone(),
                            on: n == *cursor,
                        });
                    }
                }
                if let Some(note) = self.notes.get(def.path) {
                    items.push(Item::Note(note.clone()));
                }
                if let Some(hint) = self.hints.get(def.path) {
                    items.push(Item::Hint(hint.clone()));
                }
                if let Some(offer) = self.setup_offers.get(def.path) {
                    if self.setup_armed.as_deref() == Some(offer.id.as_str()) {
                        items.extend(offer.preview.lines().map(|line| Item::Setup {
                            line: line.to_owned(),
                            chip: false,
                        }));
                    } else {
                        items.push(Item::Setup {
                            line: format!("Ctrl+L installs {}", offer.title),
                            chip: true,
                        });
                    }
                }
            }
        }
        let overflow = items.len() > body_rows;
        let list_w = list_w_total.saturating_sub(usize::from(overflow));
        let mut top = cursor_item;
        let mut bottom = cursor_item;
        if matches!(items.get(cursor_item), Some(Item::Choice { .. })) {
            // Opening a picker near the bottom must reveal its alternatives, not just its
            // selected value. Keep the setting label with the choices when the block fits.
            while top > 0
                && matches!(
                    items.get(top - 1),
                    Some(Item::Choice { .. } | Item::Filter { .. })
                )
            {
                top -= 1;
            }
            top = top.saturating_sub(1);
            while matches!(
                items.get(bottom + 1),
                Some(Item::Choice { .. } | Item::Filter { .. })
            ) {
                bottom += 1;
            }
            if bottom - top + 1 > body_rows {
                top = cursor_item;
                bottom = cursor_item;
            }
        }
        if top > 0 && matches!(items.get(top - 1), Some(Item::Header(_))) {
            top -= 1;
        }
        while matches!(
            items.get(bottom + 1),
            Some(Item::Note(_) | Item::Hint(_) | Item::Setup { .. } | Item::Filter { .. })
        ) {
            bottom += 1;
        }
        if top < self.scroll {
            self.scroll = top;
        }
        if bottom >= self.scroll + body_rows {
            self.scroll = bottom + 1 - body_rows.max(1);
        }
        self.scroll = self.scroll.min(items.len().saturating_sub(body_rows));

        let query_positions = |row: Row| -> Vec<usize> {
            match row {
                Row::Setting(i) if searching => {
                    let def = &crate::config::schema::registry()[i];
                    let label_len = def.label.chars().count();
                    search_match(&self.search, &haystack(def), def.description)
                        .map(|(_, p)| p.into_iter().filter(|p| *p < label_len).collect())
                        .unwrap_or_default()
                }
                Row::Link(i) if searching => {
                    let label = LINKS[i].0;
                    search_match(
                        &self.search,
                        &format!("{label} {LINK_SECTION} {PROVIDERS_TAB}"),
                        LINKS[i].1,
                    )
                    .map(|(_, p)| {
                        p.into_iter()
                            .filter(|p| *p < label.chars().count())
                            .collect()
                    })
                    .unwrap_or_default()
                }
                _ => Vec::new(),
            }
        };

        let section_lines = self.section_column(theme, left_w, cursor_row, rows.len());
        for y in 0..body_rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if left_w > 0 {
                let left = section_lines.get(y).cloned().unwrap_or_default();
                let used = spans_width(&left);
                spans.extend(left);
                spans.push(Span::raw(" ".repeat(left_w.saturating_sub(used))));
                spans.push(Span::styled(
                    theme.sym(Sym::BoxV).to_owned(),
                    theme.fg(Tok::BorderMuted),
                ));
            }
            let item = items.get(self.scroll + y);
            let list_line = match item {
                None if rows.is_empty() && y == 0 => {
                    let msg = if searching {
                        format!(
                            " No settings match \"{}\". Esc clears the search.",
                            text::one_line(&self.search)
                        )
                    } else {
                        " Nothing here".to_owned()
                    };
                    filled(
                        vec![Span::styled(msg, theme.fg(Tok::Muted))],
                        list_w,
                        Style::default(),
                    )
                }
                None => filled(vec![], list_w, Style::default()),
                Some(Item::Header(name)) => filled(
                    vec![Span::raw(" "), chrome::label(theme, name)],
                    list_w,
                    Style::default(),
                ),
                Some(Item::Note(note)) => filled(
                    vec![Span::styled(
                        format!("    {} {}", theme.sym(Sym::Cross2), text::one_line(note)),
                        theme.fg(Tok::Error),
                    )],
                    list_w,
                    Style::default(),
                ),
                Some(Item::Hint(hint)) => filled(
                    vec![Span::styled(
                        format!("    {} {}", theme.sym(Sym::Bullet), text::one_line(hint)),
                        theme.fg(Tok::Muted),
                    )],
                    list_w,
                    Style::default(),
                ),
                Some(Item::Setup { line, chip }) => {
                    let mut spans = vec![Span::raw("    ")];
                    if *chip {
                        spans.push(status_chip(theme, Status::Missing, "available"));
                        spans.push(Span::styled(
                            format!(" {}", text::one_line(line)),
                            theme.fg(Tok::Muted),
                        ));
                    } else {
                        spans.push(Span::styled(text::one_line(line), theme.fg(Tok::Muted)));
                    }
                    filled(spans, list_w, Style::default())
                }
                Some(Item::Filter {
                    editor,
                    shown,
                    total,
                }) => {
                    let count = if editor.text.trim().is_empty() {
                        format!("{total} options")
                    } else {
                        format!("{shown} of {total}")
                    };
                    let mut spans = vec![Span::styled("      Filter: ", theme.fg(Tok::Muted))];
                    let room = list_w.saturating_sub(spans_width(&spans) + text::width(&count) + 3);
                    spans.extend(editor_spans(theme, editor, room, false));
                    spans.push(Span::styled(format!("   {count}"), theme.fg(Tok::Muted)));
                    filled(spans, list_w, Style::default())
                }
                Some(Item::Choice { name, on }) => {
                    let pointer = if *on {
                        theme.sym(Sym::RadioOn)
                    } else {
                        theme.sym(Sym::RadioOff)
                    };
                    let fill = if *on {
                        selected_fill(theme)
                    } else {
                        Style::default()
                    };
                    let style = if *on { fill } else { theme.fg(Tok::Muted) };
                    filled(
                        vec![Span::styled(
                            format!("      {pointer} {}", text::one_line(name)),
                            style,
                        )],
                        list_w,
                        fill,
                    )
                }
                Some(Item::Row(i)) => {
                    let selected = *i == self.cursor;
                    self.row_line(
                        theme,
                        rows[*i],
                        selected,
                        list_w,
                        &query_positions(rows[*i]),
                    )
                }
            };
            spans.extend(list_line.spans);
            if overflow {
                spans.push(scroll_cell(theme, y, body_rows, items.len(), self.scroll));
            }
            lines.push(Line::from(spans));
        }

        if show_desc {
            let wanted = if show_desc2 { 2 } else { 1 };
            // The explanation leads; the path, source and restart state follow it, so a one-row
            // panel still says what the setting does. Save feedback and errors come first.
            let mut head: Vec<(String, Tok)> = Vec::new();
            let mut meta: Option<(String, Tok)> = None;
            if let Some(error) = &self.load_error {
                head.push((error.clone(), Tok::Error));
            }
            match cursor_row {
                Some(Row::Setting(i)) => {
                    let def = &crate::config::schema::registry()[i];
                    if def.path == "memory.backend" && matches!(self.mode, Mode::Browse) {
                        // What the selected backend does and, if setting one up, what remains.
                        if let Some(feedback) = &self.feedback {
                            head.push((feedback.clone(), Tok::Success));
                        }
                        let backend = self.memory_backend();
                        let models = ctx.keys.label(Action::ModelSelect);
                        head.push((memory_summary(&backend, &models), Tok::Text));
                    } else if matches!(self.mode, Mode::Edit { .. }) {
                        head.push((input_hint(def.kind_for(&self.effective)), Tok::Muted));
                    } else {
                        if let Some(feedback) = &self.feedback {
                            head.push((feedback.clone(), Tok::Success));
                        }
                        let dot = format!(" {} ", theme.sym(Sym::Dot));
                        meta = Some((
                            format!(
                                "{}{dot}Using {} value{}",
                                def.path,
                                self.source(def.path),
                                if def.restart {
                                    format!("{dot}Restart required")
                                } else {
                                    String::new()
                                }
                            ),
                            Tok::Dim,
                        ));
                    }
                    head.extend(
                        text::wrap_words(def.description, w.saturating_sub(2).max(1))
                            .into_iter()
                            .map(|l| (l, Tok::Muted)),
                    );
                }
                Some(Row::Advanced(_, _, count)) => head.push((
                    format!("{count} tuning settings: timeouts, limits and budgets. Search also finds them."),
                    Tok::Muted,
                )),
                Some(Row::Link(i)) => head.extend(
                    text::wrap_words(LINKS[i].1, w.saturating_sub(2).max(1))
                        .into_iter()
                        .map(|l| (l, Tok::Muted)),
                ),
                None => {}
            }
            // With two rows, the meta line takes the second so the path and source stay visible.
            let lead = if wanted > 1 && meta.is_some() {
                wanted - 1
            } else {
                wanted
            };
            let mut desc: Vec<(String, Tok)> = head.into_iter().take(lead).collect();
            if let Some(meta) = meta
                && (wanted > 1 || desc.is_empty())
            {
                desc.push(meta);
            }
            for n in 0..wanted {
                lines.push(match desc.get(n) {
                    Some((line, tok)) => Line::from(fit(
                        vec![Span::styled(
                            format!(" {}", text::one_line(line)),
                            theme.fg(*tok),
                        )],
                        w,
                    )),
                    None => Line::default(),
                });
            }
        }
        if show_preview && let Some(preview) = preview {
            let mut spans = vec![Span::styled(preview_label, theme.fg(Tok::Muted))];
            spans.extend(fit(preview.spans, preview_room));
            lines.push(Line::from(fit(spans, w)));
        }
        if show_footer {
            lines.push(hint_line(theme, &self.footer(ctx.keys), w));
        }
        lines
    }

    fn tab_bar(&self, theme: &Theme, w: usize) -> Line<'static> {
        let names: Vec<&str> = self
            .tabs
            .iter()
            .map(|(t, _)| *t)
            .chain(std::iter::once(PROVIDERS_TAB))
            .collect();
        // Plain words, two cells apart: the active tab's accent and underline mark it, so icons
        // would only add noise.
        tab_strip(theme, &names, self.tab, self.search.is_empty(), w)
    }

    /// The search field reads as an input: a muted label, then the query with its caret, or a
    /// placeholder. It carries no selection marker; only the selected row does.
    fn search_line(&self, theme: &Theme, w: usize) -> Line<'static> {
        let label = Span::styled(" Search: ", theme.fg(Tok::Muted));
        let spans = if self.search.is_empty() {
            vec![
                label,
                Span::styled(" ", theme.fg(Tok::Text).add_modifier(Modifier::REVERSED)),
                Span::styled("type a setting name, path or keyword", theme.fg(Tok::Dim)),
            ]
        } else {
            vec![
                label,
                Span::styled(text::one_line(&self.search), theme.fg(Tok::Text)),
                Span::styled(" ", theme.fg(Tok::Text).add_modifier(Modifier::REVERSED)),
            ]
        };
        Line::from(fit(spans, w))
    }

    fn section_column(
        &self,
        theme: &Theme,
        width: usize,
        cursor_row: Option<Row>,
        hits: usize,
    ) -> Vec<Vec<Span<'static>>> {
        if width == 0 {
            return Vec::new();
        }
        let mut out: Vec<Vec<Span<'static>>> = Vec::new();
        if !self.search.is_empty() {
            out.push(vec![chrome::label(theme, " Results")]);
            out.push(vec![Span::styled(
                format!(" {hits} {}", if hits == 1 { "match" } else { "matches" }),
                theme.fg(Tok::Muted),
            )]);
            return out;
        }
        let sections: Vec<&str> = match self.tabs.get(self.tab) {
            Some((_, s)) => s.clone(),
            None => vec![LINK_SECTION],
        };
        let active = cursor_row.map(Self::section_of);
        for section in sections {
            let on = Some(section) == active;
            let label = text::ellipsize(section, width.saturating_sub(3), theme.ascii());
            // The section holding the selection is named in the accent; the selection marker
            // belongs to the selected row alone.
            out.push(if on {
                vec![Span::styled(
                    format!("   {label}"),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled(format!("   {label}"), theme.fg(Tok::Muted))]
            });
        }
        out
    }

    fn row_line(
        &self,
        theme: &Theme,
        row: Row,
        selected: bool,
        w: usize,
        positions: &[usize],
    ) -> Line<'static> {
        let fill = if selected {
            selected_fill(theme)
        } else {
            Style::default()
        };
        let base = |tok: Tok| theme.fg(tok).patch(fill);
        let pointer = if selected {
            theme.sym(Sym::Pointer)
        } else {
            " "
        };
        let mut spans = vec![Span::styled(format!("{pointer} "), base(Tok::Accent))];
        let label_w = (w * 45 / 100).clamp(8, 32).min(w.saturating_sub(2));
        let (label, value_spans, tags, search_tag): (
            String,
            Vec<Span<'static>>,
            Vec<(String, Tok)>,
            String,
        ) = match row {
            Row::Link(i) => (
                LINKS[i].0.to_owned(),
                vec![Span::styled("open", base(Tok::Muted))],
                Vec::new(),
                format!("{PROVIDERS_TAB}{}{LINK_SECTION}", theme.sym(Sym::SepSlash)),
            ),
            Row::Advanced(tab, section, count) => {
                let verb = if self.advanced_open.contains(&(tab, section)) {
                    "Hide"
                } else {
                    "Show"
                };
                (
                    format!("{verb} advanced ({count})"),
                    Vec::new(),
                    Vec::new(),
                    format!("{tab}{}{section}", theme.sym(Sym::SepSlash)),
                )
            }
            Row::Setting(i) => {
                let def = &crate::config::schema::registry()[i];
                let editing = match &self.mode {
                    Mode::Edit { def: d, editor } if d.path == def.path => Some(editor),
                    _ => None,
                };
                let value = self.current(def);
                let source = self.source(def.path);
                let value_spans = if let Some(editor) = editing {
                    let room = w.saturating_sub(2 + label_w + 2);
                    let mut s = vec![Span::styled("[", base(Tok::Muted))];
                    s.extend(editor_spans(theme, editor, room, false));
                    s.push(Span::styled("]", base(Tok::Muted)));
                    s
                } else {
                    let text = if def.path == EMBEDDING_MODEL_PATH {
                        self.embedding_row_text(&value)
                    } else {
                        value_text(def, &value)
                    };
                    match def.kind_for(&self.effective) {
                        Kind::Bool => {
                            let on = value.as_bool().unwrap_or(false);
                            vec![Span::styled(
                                format!(
                                    "{} {text}",
                                    theme.sym(if on { Sym::RadioOn } else { Sym::RadioOff })
                                ),
                                base(if on { Tok::Success } else { Tok::Muted }),
                            )]
                        }
                        _ => vec![Span::styled(text, base(Tok::Text))],
                    }
                };
                let mut tags: Vec<(String, Tok)> = Vec::new();
                if editing.is_none() {
                    // Defaults carry no tag; an override names where it is set, in the accent.
                    if source != "default" {
                        tags.push((format!("{} {source}", theme.sym(Sym::Bullet)), Tok::Accent));
                    }
                    if def.restart {
                        tags.push(("restart".to_owned(), Tok::Warning));
                    }
                }
                (
                    def.label.to_owned(),
                    value_spans,
                    tags,
                    format!("{}{}{}", def.tab, theme.sym(Sym::SepSlash), def.section),
                )
            }
        };
        let label = text::one_line(&label);
        let shown = text::ellipsize(&label, label_w.saturating_sub(1), theme.ascii());
        let label_style = if selected {
            base(Tok::Text).add_modifier(Modifier::BOLD)
        } else {
            base(Tok::Text)
        };
        let hl = match_style(theme, label_style);
        let mut label_spans = if positions.is_empty() {
            vec![Span::styled(shown.clone(), label_style)]
        } else {
            highlighted(&shown, positions, label_style, hl)
        };
        spans.append(&mut label_spans);
        spans.push(Span::styled(
            " ".repeat(label_w.saturating_sub(text::width(&shown))),
            base(Tok::Text),
        ));

        let room = w.saturating_sub(2 + label_w);
        let value_w = spans_width(&value_spans);
        // In search mode the tab/section replaces the tags: it tells where a hit lives.
        let mut tag_spans: Vec<Span<'static>> = Vec::new();
        if !self.search.is_empty() {
            tag_spans.push(Span::styled(format!("  {search_tag}"), base(Tok::Muted)));
        } else {
            let mut tags = tags;
            loop {
                let width: usize = tags.iter().map(|(t, _)| text::width(t) + 1).sum::<usize>() + 1;
                if tags.is_empty() || value_w + width <= room {
                    break;
                }
                // Drop the least important tag first: restart, then the source.
                if let Some(p) = tags.iter().position(|(t, _)| t == "restart") {
                    tags.remove(p);
                } else {
                    tags.clear();
                }
            }
            let pad_to = (room * 55 / 100)
                .max(value_w)
                .min(room.saturating_sub(room / 4));
            let pad = pad_to.saturating_sub(value_w);
            if !tags.is_empty() {
                tag_spans.push(Span::styled(" ".repeat(pad.max(1)), base(Tok::Text)));
            }
            for (t, tok) in tags {
                tag_spans.push(Span::styled(format!("{t} "), base(tok)));
            }
        }
        let value_spans = if value_w > room {
            fit(value_spans, room.saturating_sub(1))
                .into_iter()
                .chain(std::iter::once(Span::styled(
                    theme.sym(Sym::Ellipsis).to_owned(),
                    base(Tok::Muted),
                )))
                .collect()
        } else {
            value_spans
        };
        spans.extend(value_spans);
        spans.extend(tag_spans);
        filled(spans, w, fill)
    }

    fn footer(&self, keys: &Keymap) -> Vec<String> {
        let pair = |a, b, what| {
            let label = pair_label(keys, a, b);
            if label.is_empty() {
                String::new()
            } else {
                format!("{label} {what}")
            }
        };
        match &self.mode {
            Mode::Edit { def, .. } => {
                let numeric = matches!(
                    def.kind_for(&self.effective),
                    Kind::Int { .. }
                        | Kind::AutoInt { .. }
                        | Kind::Float { .. }
                        | Kind::AutoFloat { .. }
                );
                vec![
                    hint(keys, Action::SelectConfirm, "save"),
                    if numeric {
                        pair(Action::SelectUp, Action::SelectDown, "adjust")
                    } else {
                        String::new()
                    },
                    hint(keys, Action::SelectCancel, "cancel"),
                ]
            }
            Mode::Pick {
                def,
                choices,
                filter,
                ..
            } => {
                let filtering = !filter.text.is_empty();
                let matched = !self.pick_visible(def, choices, &filter.text).is_empty();
                let mut hints = Vec::new();
                if matched {
                    hints.push(hint(keys, Action::SelectConfirm, "save"));
                    hints.push(pair(
                        Action::SelectUp,
                        Action::SelectDown,
                        if matches!(def.kind_for(&self.effective), Kind::Theme) {
                            "preview"
                        } else {
                            "choose"
                        },
                    ));
                }
                if choices.len() > PICK_ROWS || filtering {
                    hints.push("Type filter".to_owned());
                }
                hints.push(hint(
                    keys,
                    Action::SelectCancel,
                    if filtering { "clear filter" } else { "cancel" },
                ));
                hints
            }
            Mode::Browse => {
                let verb = match self.current_row() {
                    Some(Row::Link(_)) => "open",
                    Some(Row::Advanced(tab, section, _)) => {
                        if self.advanced_open.contains(&(tab, section)) {
                            "hide"
                        } else {
                            "show"
                        }
                    }
                    Some(Row::Setting(i)) => match crate::config::schema::registry()[i].kind {
                        Kind::Bool => "toggle",
                        Kind::AutoBool | Kind::Enum(_) | Kind::Choice(_) | Kind::Theme => "choose",
                        _ => "edit",
                    },
                    None => "select",
                };
                let mut hints = vec![
                    hint(keys, Action::SelectConfirm, verb),
                    pair(Action::SelectUp, Action::SelectDown, "move"),
                    if self.search.is_empty() {
                        pair(Action::TabPrev, Action::TabNext, "tabs")
                    } else {
                        // Tab does not switch tabs while searching, so do not advertise it then.
                        String::new()
                    },
                    hint(keys, Action::SectionNext, "sections"),
                    "Ctrl+S scope".to_owned(),
                    "Del clear override".to_owned(),
                    if self.setup_offers.is_empty() {
                        String::new()
                    } else {
                        "Ctrl+L set up".to_owned()
                    },
                    hint(
                        keys,
                        Action::SelectCancel,
                        if self.search.is_empty() {
                            "close"
                        } else {
                            "clear search"
                        },
                    ),
                ];
                hints.retain(|hint| !hint.is_empty());
                hints
            }
        }
    }
}

#[cfg(test)]
mod tests;

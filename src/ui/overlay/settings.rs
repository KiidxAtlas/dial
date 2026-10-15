//! The `/settings` panel: tab bar, section column, grouped value list, description, live status-line
//! preview and key hints. It never touches configuration: every change leaves as an
//! [`OverlayEvent`] and the displayed value only changes once the controller answers via
//! [`SettingsOverlay::settled`].
//!
//! This file also holds the small drawing helpers shared by the model and provider panels.
use super::{DrawCtx, OverlayEvent};
use crate::{
    llm::model_config::UiConfig,
    storage::settings_store::Scope,
    ui::{
        chrome::{self, Section, Status, status_chip},
        editor::Editor,
        keys::{Action, Keymap},
        settings_schema::{self, Kind, SettingDef},
        text,
        theme::{Sym, Theme, Tok},
    },
};
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

/// Style of a tab-bar cell: the active tab is amber, bold and underlined; the others are muted words.
pub(super) fn tab_style(theme: &Theme, active: bool) -> Style {
    if active {
        theme
            .fg(Tok::Accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        theme.fg(Tok::Muted)
    }
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
        Span::styled(
            theme.sym(Sym::ScrollTrack).to_owned(),
            theme.fg(Tok::BorderMuted),
        )
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

#[cfg(test)]
pub(super) mod testkit {
    use crate::{
        llm::model_config::UiConfig,
        ui::{keys::Keymap, status::StatusData, theme::Theme},
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{
        Frame, Terminal, backend::TestBackend, buffer::Buffer, style::Style, text::Line,
    };

    pub fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    pub fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }
    pub fn ch(c: char) -> Event {
        key(KeyCode::Char(c))
    }
    pub fn typed(s: &str) -> Vec<Event> {
        s.chars().map(ch).collect()
    }

    pub fn status_preview(_: &UiConfig, _: u16) -> Line<'static> {
        Line::from("PREVIEW")
    }

    pub struct Rig {
        pub theme: Theme,
        pub keys: Keymap,
        pub status: StatusData,
    }
    impl Rig {
        pub fn new() -> Self {
            Rig {
                theme: Theme::dark(),
                keys: Keymap::defaults(),
                status: StatusData::default(),
            }
        }
        pub fn with_theme(theme: Theme) -> Self {
            Rig {
                theme,
                ..Rig::new()
            }
        }
    }

    pub fn render(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(draw).unwrap();
        terminal.backend().buffer().clone()
    }

    pub fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    pub fn screen(buffer: &Buffer) -> String {
        rows(buffer).join("\n")
    }

    /// Style of the first cell of `needle` found on the screen.
    pub fn style_of(buffer: &Buffer, needle: &str) -> Option<Style> {
        for (y, row) in rows(buffer).iter().enumerate() {
            if let Some(byte) = row.find(needle) {
                let x = row[..byte].chars().count();
                return Some(buffer[(x as u16, y as u16)].style());
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Settings panel
// ---------------------------------------------------------------------------------------------

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
    },
}

enum Item {
    Header(&'static str),
    Row(usize),
    Note(String),
    /// A dependency-setup line: the first line of an offer carries the `missing` chip.
    Setup {
        line: String,
        chip: bool,
    },
    Choice {
        name: String,
        on: bool,
    },
}

/// A missing dependency that a setting path needs: what to install and the plan to show first.
pub struct SetupOffer {
    pub id: String,
    pub title: String,
    pub preview: String,
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
}

/// Plain-language summary of each memory backend, shown on the Backend row.
fn memory_summary(backend: &str) -> &'static str {
    match backend {
        "off" => {
            "Memory is off: nothing carries over between sessions and the memory tools are hidden."
        }
        "provenance" => {
            "Saves facts the agent learns, with history and expiry, found again by keyword. Needs no setup."
        }
        "local" => {
            "Summarises your past sessions into project memory, using the model on the Memory role (Ctrl+K, Roles)."
        }
        "hindsight" => {
            "Connects to a Hindsight memory service you run. Needs its URL, and an API key variable for a remote server."
        }
        "sharpshooter" => {
            "Keeps project decision notes, written by the model on the Memory role (Ctrl+K, Roles)."
        }
        "semantic" => {
            "Recalls facts by keyword, and by meaning when an embedding service is set. Needs no setup; with no service, nothing leaves this machine."
        }
        _ => "",
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

/// Required fields of `backend` that are still empty, as setting paths, in the order to fill them.
/// Mirrors `memory_backend::config::validate`, which rejects saving the backend without them.
fn memory_needs(backend: &str, get: impl Fn(&str) -> Value) -> Vec<&'static str> {
    let empty = |path: &str| get(path).as_str().is_none_or(|v| v.trim().is_empty());
    let mut needs = Vec::new();
    match backend {
        "hindsight" => {
            let endpoint = get("memory.hindsight.endpoint");
            let endpoint = endpoint.as_str().unwrap_or("").trim();
            if endpoint.is_empty() {
                needs.push("memory.hindsight.endpoint");
            } else if empty("memory.hindsight.api_key_env") && !is_loopback_url(endpoint) {
                needs.push("memory.hindsight.api_key_env");
            }
        }
        _ => {}
    }
    needs
}

/// The display name of a `memory.backend` choice (`Hindsight service`).
fn label_for_choice(backend: &str) -> &'static str {
    settings_schema::registry()
        .iter()
        .find(|d| d.path == "memory.backend")
        .and_then(|d| match d.kind {
            Kind::Enum(options) => options.iter().find(|(v, _)| *v == backend).map(|(_, l)| *l),
            _ => None,
        })
        .unwrap_or("That backend")
}

fn label_of(path: &str) -> &'static str {
    settings_schema::registry()
        .iter()
        .find(|d| d.path == path)
        .map_or("", |d| d.label)
}

fn haystack(def: &SettingDef) -> String {
    format!("{} {} {} {}", def.label, def.section, def.tab, def.path)
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
        (Kind::Bool, Value::Boolean(b)) => if *b { "on" } else { "off" }.to_owned(),
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
            tabs: settings_schema::tabs(),
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
            feedback: None,
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
    fn learning_setting(def: &SettingDef) -> bool {
        def.tab == "Improvement"
            || def.path.starts_with("laya.")
            || def.path.starts_with("memory.autolearn")
            || def.path.starts_with("memory.learn_")
    }
    fn scope_shown(&self, def: &SettingDef) -> bool {
        let learning = Self::learning_setting(def);
        (!self.coding_only || !learning) && (!self.learning_only || learning)
    }
    fn filter_tabs(&mut self) {
        self.tabs = settings_schema::tabs()
            .into_iter()
            .filter_map(|(tab, sections)| {
                let sections = sections
                    .into_iter()
                    .filter(|section| {
                        settings_schema::registry().iter().any(|def| {
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

    pub fn replace_values(&mut self, values: Table, project_values: Table, global_values: Table) {
        self.effective = values;
        self.project = project_values;
        self.global = global_values;
        self.pending.clear();
        self.notes.clear();
    }

    /// The controller's verdict for the last `Set`/`Unset` of `path`.
    pub fn settled(&mut self, path: &str, result: Result<Value, String>) {
        let pending = self.pending.remove(path);
        self.feedback = None;
        match result {
            Ok(value) => {
                match pending {
                    Some(Pending::Set(v, scope)) => {
                        settings_schema::set(self.layer(scope), path, v);
                        self.feedback = Some(
                            if scope == Scope::Global
                                && settings_schema::get(&self.project, path).is_some()
                            {
                                "Saved globally; this project's override still applies".to_owned()
                            } else {
                                format!("Saved to {} settings", scope_name(scope))
                            },
                        );
                    }
                    Some(Pending::Unset(scope)) => {
                        settings_schema::unset(self.layer(scope), path);
                        self.feedback = Some(format!("Removed {} override", scope_name(scope)));
                    }
                    None => {}
                }
                settings_schema::set(&mut self.effective, path, value);
                if path == "memory.backend" {
                    self.memory_intent = None;
                }
                self.notes.remove(path);
                if let Some(intent) = self.memory_intent.clone()
                    && path.starts_with(memory_section(&intent).unwrap_or("\0"))
                {
                    match memory_needs(&intent, |p| self.current_path(p)).first() {
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
            }
            Err(message) => {
                self.notes.insert(path.to_owned(), text::one_line(&message));
            }
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
        if settings_schema::get(&self.project, path).is_some() {
            "project"
        } else if settings_schema::get(&self.global, path).is_some() {
            "global"
        } else {
            "default"
        }
    }

    /// What `path` falls back to once it is removed from `scope`.
    fn after_unset(&self, def: &SettingDef, scope: Scope) -> Option<Value> {
        settings_schema::get(
            self.layer_ref(super::settings::other_scope(scope)),
            def.path,
        )
        .cloned()
        .or_else(|| def.default_value())
    }

    fn current_path(&self, path: &str) -> Value {
        settings_schema::registry()
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
        if let Some(at) = self.visible().iter().position(
            |r| matches!(r, Row::Setting(i) if settings_schema::registry()[*i].path == path),
        ) {
            self.cursor = at;
        }
    }

    fn current(&self, def: &SettingDef) -> Value {
        match self.pending.get(def.path) {
            Some(Pending::Set(v, _)) => v.clone(),
            Some(Pending::Unset(scope)) => self
                .after_unset(def, *scope)
                .unwrap_or_else(|| Value::String(String::new())),
            None => settings_schema::get(&self.effective, def.path)
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
                Pending::Set(v, _) => settings_schema::set(&mut table, path, v.clone()),
                Pending::Unset(scope) => {
                    match settings_schema::registry()
                        .iter()
                        .find(|d| d.path == path)
                        .and_then(|d| self.after_unset(d, *scope))
                    {
                        Some(v) => settings_schema::set(&mut table, path, v),
                        None => {
                            settings_schema::unset(&mut table, path);
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
        let defs = settings_schema::registry();
        if !self.search.is_empty() {
            let mut hits: Vec<(i32, Row)> = Vec::new();
            for (i, def) in defs
                .iter()
                .enumerate()
                .filter(|(_, def)| self.scope_shown(def))
            {
                if let Some((score, _)) = text::fuzzy(&self.search, &haystack(def)) {
                    hits.push((score, Row::Setting(i)));
                }
            }
            for (i, (label, ..)) in LINKS.iter().enumerate() {
                if let Some((score, _)) = text::fuzzy(
                    &self.search,
                    &format!("{label} {LINK_SECTION} {PROVIDERS_TAB}"),
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
                        .filter(|(_, d)| in_section(*d) && !settings_schema::is_advanced(d.path))
                        .map(|(i, _)| Row::Setting(i));
                    let advanced: Vec<usize> = defs
                        .iter()
                        .enumerate()
                        .filter(|(_, d)| in_section(*d) && settings_schema::is_advanced(d.path))
                        .map(|(i, _)| i)
                        .collect();
                    rows.extend(basic);
                    if advanced.is_empty() {
                        continue;
                    }
                    let open = self.advanced_open.contains(&(*tab, *section));
                    rows.push(Row::Advanced(*tab, *section, advanced.len()));
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
            Row::Setting(i) => settings_schema::registry()[i].section,
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
                    Mode::Pick { .. } => {}
                }
                OverlayEvent::None
            }
            _ => OverlayEvent::None,
        }
    }

    fn browse_key(&mut self, key: &KeyEvent, keys: &Keymap) -> OverlayEvent {
        self.feedback = None;
        if is_ctrl(key, 's') {
            self.scope = other_scope(self.scope);
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
        let path = settings_schema::registry()[i].path;
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

    fn emit_set(&mut self, def: &SettingDef, value: Value) -> OverlayEvent {
        self.feedback = None;
        self.notes.remove(def.path);
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
            Some(Row::Setting(i)) => &settings_schema::registry()[i],
            None => return OverlayEvent::None,
        };
        let current = self.current(def);
        match def.kind_for(&self.effective) {
            Kind::Bool => {
                let next = !current.as_bool().unwrap_or(false);
                self.emit_set(def, Value::Boolean(next))
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
        let def = &settings_schema::registry()[i];
        if settings_schema::get(self.layer_ref(self.scope), def.path).is_some() {
            self.notes.remove(def.path);
            self.pending
                .insert(def.path.to_owned(), Pending::Unset(self.scope));
            OverlayEvent::Unset {
                path: def.path.to_owned(),
                scope: self.scope,
            }
        } else {
            if settings_schema::get(self.layer_ref(other_scope(self.scope)), def.path).is_some() {
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
        let Mode::Pick {
            def,
            choices,
            cursor,
        } = &mut self.mode
        else {
            return OverlayEvent::None;
        };
        let def: &'static SettingDef = def;
        let last = choices.len().saturating_sub(1);
        if keys.matches(Action::SelectCancel, key) {
            self.mode = Mode::Browse;
            if def.path == "memory.backend" {
                self.memory_intent = None;
            }
            return if matches!(def.kind_for(&self.effective), Kind::Theme) {
                OverlayEvent::PreviewTheme(None)
            } else {
                OverlayEvent::None
            };
        }
        if keys.matches(Action::SelectConfirm, key) {
            let chosen = choices[*cursor].clone();
            let unchanged = self.current(def).as_str() == Some(chosen.as_str());
            self.mode = Mode::Browse;
            if def.path == "memory.backend" {
                let needs = memory_needs(&chosen, |p| self.current_path(p));
                if let Some(first) = needs.first() {
                    // Saving the backend now would fail validation: collect its fields first.
                    let labels: Vec<_> = needs.iter().map(|p| label_of(p)).collect();
                    self.memory_intent = Some(chosen.clone());
                    self.feedback = Some(format!(
                        "To switch, first fill: {}. You'll come back here when they're set.",
                        labels.join(", ")
                    ));
                    self.move_to_path(first);
                    return OverlayEvent::None;
                }
                self.memory_intent = None;
            }
            return if unchanged {
                if matches!(def.kind_for(&self.effective), Kind::Theme) {
                    OverlayEvent::PreviewTheme(None)
                } else {
                    OverlayEvent::None
                }
            } else {
                self.emit_set(def, Value::String(chosen))
            };
        }
        let moved = if keys.matches(Action::SelectUp, key) {
            *cursor = if *cursor == 0 { last } else { *cursor - 1 };
            true
        } else if keys.matches(Action::SelectDown, key) {
            *cursor = if *cursor >= last { 0 } else { *cursor + 1 };
            true
        } else if keys.matches(Action::SelectPageUp, key) {
            *cursor = 0;
            true
        } else if keys.matches(Action::SelectPageDown, key) {
            *cursor = last;
            true
        } else {
            false
        };
        if moved && matches!(def.kind_for(&self.effective), Kind::Theme) {
            OverlayEvent::PreviewTheme(Some(choices[*cursor].clone()))
        } else {
            OverlayEvent::None
        }
    }

    // ---- drawing --------------------------------------------------------------------------

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let scope = match self.scope {
            Scope::Global => "All projects",
            Scope::Project => "This project",
        };
        let extra = format!("{} Save to: {scope}  (Ctrl+S)", ctx.theme.sym(Sym::Dot));
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
        let appearance = matches!(cursor_row, Some(Row::Setting(i)) if settings_schema::registry()[i].tab == "Appearance");
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
        let show_preview = appearance && grant();
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
                let def = &settings_schema::registry()[*s];
                if let Mode::Pick {
                    def: picking,
                    choices,
                    cursor,
                } = &self.mode
                    && picking.path == def.path
                    && i == self.cursor
                {
                    let first = cursor
                        .saturating_sub(3)
                        .min(choices.len().saturating_sub(7));
                    for (n, name) in choices.iter().enumerate().skip(first).take(7) {
                        if n == *cursor {
                            cursor_item = items.len();
                        }
                        items.push(Item::Choice {
                            name: value_text(def, &Value::String(name.clone())),
                            on: n == *cursor,
                        });
                    }
                }
                if let Some(note) = self.notes.get(def.path) {
                    items.push(Item::Note(note.clone()));
                } else if let Some(hint) = self.hints.get(def.path) {
                    items.push(Item::Note(hint.clone()));
                }
                if let Some(offer) = self.setup_offers.get(def.path) {
                    if self.setup_armed.as_deref() == Some(offer.id.as_str()) {
                        items.extend(offer.preview.lines().map(|line| Item::Setup {
                            line: line.to_owned(),
                            chip: false,
                        }));
                    } else {
                        items.push(Item::Setup {
                            line: format!(
                                "{} is not installed; press Ctrl+L to set it up",
                                offer.title
                            ),
                            chip: true,
                        });
                    }
                }
            }
        }
        let overflow = items.len() > body_rows;
        let list_w = list_w_total.saturating_sub(usize::from(overflow));
        let mut top = cursor_item;
        if top > 0 && matches!(items.get(top - 1), Some(Item::Header(_))) {
            top -= 1;
        }
        let mut bottom = cursor_item;
        while matches!(
            items.get(bottom + 1),
            Some(Item::Note(_) | Item::Setup { .. })
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
                    let def = &settings_schema::registry()[i];
                    let label_len = def.label.chars().count();
                    text::fuzzy(&self.search, &haystack(def))
                        .map(|(_, p)| p.into_iter().filter(|p| *p < label_len).collect())
                        .unwrap_or_default()
                }
                Row::Link(i) if searching => {
                    let label = LINKS[i].0;
                    text::fuzzy(
                        &self.search,
                        &format!("{label} {LINK_SECTION} {PROVIDERS_TAB}"),
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
                        format!(" No settings match \"{}\"", text::one_line(&self.search))
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
                Some(Item::Setup { line, chip }) => {
                    let mut spans = vec![Span::raw("    ")];
                    if *chip {
                        spans.push(status_chip(theme, Status::Missing, "missing"));
                        spans.push(Span::styled(
                            format!(" {}", text::one_line(line)),
                            theme.fg(Tok::Muted),
                        ));
                    } else {
                        spans.push(Span::styled(text::one_line(line), theme.fg(Tok::Muted)));
                    }
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
            let mut desc: Vec<(String, Tok)> = Vec::new();
            match cursor_row {
                Some(Row::Setting(i)) => {
                    let def = &settings_schema::registry()[i];
                    if def.path == "memory.backend" && matches!(self.mode, Mode::Browse) {
                        // What the selected backend does and, if setting one up, what remains.
                        if let Some(feedback) = &self.feedback {
                            desc.push((feedback.clone(), Tok::Success));
                        }
                        let backend = self.memory_backend();
                        desc.push((memory_summary(&backend).to_owned(), Tok::Text));
                    } else if matches!(self.mode, Mode::Edit { .. }) {
                        desc.push((input_hint(def.kind_for(&self.effective)), Tok::Muted));
                    } else if show_desc2 || self.feedback.is_some() {
                        let detail = self.feedback.clone().unwrap_or_else(|| {
                            format!(
                                "{}  |  Using {} value{}",
                                def.path,
                                self.source(def.path),
                                if def.restart {
                                    "  |  Restart required"
                                } else {
                                    ""
                                }
                            )
                        });
                        desc.push((
                            detail,
                            if self.feedback.is_some() {
                                Tok::Success
                            } else {
                                Tok::Dim
                            },
                        ));
                    }
                    desc.extend(
                        text::wrap_words(def.description, w.saturating_sub(2).max(1))
                            .into_iter()
                            .map(|l| (l, Tok::Muted)),
                    );
                }
                Some(Row::Advanced(_, _, count)) => desc.push((
                    format!("{count} tuning settings: timeouts, limits and budgets. Search also finds them."),
                    Tok::Muted,
                )),
                Some(Row::Link(i)) => desc.extend(
                    text::wrap_words(LINKS[i].1, w.saturating_sub(2).max(1))
                        .into_iter()
                        .map(|l| (l, Tok::Muted)),
                ),
                None => {}
            }
            desc.truncate(wanted);
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
        if show_preview {
            let label = " Preview: ";
            let mut spans = vec![Span::styled(label, theme.fg(Tok::Muted))];
            let room = w.saturating_sub(label.len());
            if room > 0 {
                let line = (ctx.preview_status)(
                    &self.preview_config(),
                    room.min(usize::from(u16::MAX)) as u16,
                );
                spans.extend(fit(line.spans, room));
            }
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
        let cells: Vec<String> = names
            .iter()
            .map(|name| {
                let icon = theme.sym(match *name {
                    "Appearance" => Sym::IconSettings,
                    "Model" => Sym::IconModel,
                    "Interaction" => Sym::IconKeyboard,
                    "Tools" => Sym::IconSandbox,
                    "Memory" => Sym::IconSession,
                    "Improvement" => Sym::IconLearning,
                    _ => Sym::IconProvider,
                });
                if icon.is_empty() {
                    format!(" {name} ")
                } else {
                    format!(" {icon} {name} ")
                }
            })
            .collect();
        let widths: Vec<usize> = cells.iter().map(|c| text::width(c)).collect();
        let mut start = 0;
        while start < self.tab && widths[start..=self.tab].iter().sum::<usize>() > w {
            start += 1;
        }
        let mut spans = Vec::new();
        for (i, cell) in cells.into_iter().enumerate().skip(start) {
            let style = tab_style(theme, i == self.tab && self.search.is_empty());
            spans.push(Span::styled(cell, style));
        }
        Line::from(fit(spans, w))
    }

    fn search_line(&self, theme: &Theme, w: usize) -> Line<'static> {
        let prompt = Span::styled(
            format!(" {} ", theme.sym(Sym::Prompt)),
            theme.fg(Tok::Accent),
        );
        let spans = if self.search.is_empty() {
            vec![
                prompt,
                Span::styled("Type to search all settings", theme.fg(Tok::Dim)),
            ]
        } else {
            vec![
                prompt,
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
            out.push(if on {
                vec![
                    Span::styled(
                        format!(" {} ", theme.sym(Sym::Pointer)),
                        theme.fg(Tok::Accent),
                    ),
                    Span::styled(label, theme.fg(Tok::Accent).add_modifier(Modifier::BOLD)),
                ]
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
                let def = &settings_schema::registry()[i];
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
                    let text = value_text(def, &value);
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
            Mode::Pick { def, .. } => vec![
                hint(keys, Action::SelectConfirm, "save"),
                pair(
                    Action::SelectUp,
                    Action::SelectDown,
                    if matches!(def.kind_for(&self.effective), Kind::Theme) {
                        "preview"
                    } else {
                        "choose"
                    },
                ),
                hint(keys, Action::SelectCancel, "cancel"),
            ],
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
                    Some(Row::Setting(i)) => match settings_schema::registry()[i].kind {
                        Kind::Bool => "toggle",
                        Kind::Enum(_) | Kind::Choice(_) | Kind::Theme => "choose",
                        _ => "edit",
                    },
                    None => "select",
                };
                let mut hints = vec![
                    hint(keys, Action::SelectConfirm, verb),
                    pair(Action::SelectUp, Action::SelectDown, "move"),
                    pair(Action::TabPrev, Action::TabNext, "tabs"),
                    hint(keys, Action::SectionNext, "sections"),
                    "Ctrl+S scope".to_owned(),
                    "Del inherit".to_owned(),
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
mod tests {
    use super::testkit::*;
    use super::*;
    use std::cell::RefCell;
    #[test]
    fn coherent_runtime_learning_preferences_have_a_separate_view() {
        let mut coding = overlay().coding_only();
        assert!(!coding.tabs.iter().any(|(tab, _)| *tab == "Improvement"));
        for query in [
            "laya",
            "improve.enabled",
            "training.rank",
            "memory.autolearn",
        ] {
            coding.search = query.into();
            assert!(coding.visible().iter().all(|row| !matches!(row, Row::Setting(i) if SettingsOverlay::learning_setting(&settings_schema::registry()[*i]))));
        }
        let mut learning = overlay().learning_only();
        learning.search = "laya".into();
        assert!(learning.visible().iter().any(|row| matches!(row, Row::Setting(i) if settings_schema::registry()[*i].path == "laya.enabled")));
        learning.search = "show_tools".into();
        assert!(
            !learning
                .visible()
                .iter()
                .any(|row| matches!(row, Row::Setting(_)))
        );
    }

    fn table(src: &str) -> Table {
        toml::from_str(src).unwrap()
    }

    fn overlay() -> SettingsOverlay {
        SettingsOverlay::new(
            Table::new(),
            Table::new(),
            Table::new(),
            Scope::Global,
            vec!["dark".into(), "light".into(), "mono".into()],
        )
    }

    fn send(o: &mut SettingsOverlay, keys: &Keymap, event: Event) -> OverlayEvent {
        o.handle(&event, keys)
    }

    fn send_all(o: &mut SettingsOverlay, keys: &Keymap, events: Vec<Event>) -> OverlayEvent {
        let mut last = OverlayEvent::None;
        for e in events {
            last = o.handle(&e, keys);
        }
        last
    }

    fn draw(o: &mut SettingsOverlay, rig: &Rig, w: u16, h: u16) -> String {
        let preview = |ui: &UiConfig, width: u16| status_preview(ui, width);
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &preview,
        };
        screen(&render(w, h, |f| o.draw(f, &ctx)))
    }

    fn goto(o: &mut SettingsOverlay, keys: &Keymap, path: &str) {
        o.search.clear();
        // Advanced settings sit behind their section's "Show advanced" row; open it so the row exists.
        let def = settings_schema::registry()
            .iter()
            .find(|d| d.path == path)
            .unwrap();
        if settings_schema::is_advanced(path) {
            o.advanced_open.insert((def.tab, def.section));
        }
        o.tab = o.tabs.iter().position(|(t, _)| *t == def.tab).unwrap();
        let at = o
            .visible()
            .iter()
            .position(
                |r| matches!(r, Row::Setting(i) if settings_schema::registry()[*i].path == path),
            )
            .unwrap();
        o.cursor = at;
        let _ = keys;
    }

    fn path_at_cursor(o: &SettingsOverlay) -> &'static str {
        match o.current_row() {
            Some(Row::Setting(i)) => settings_schema::registry()[i].path,
            _ => "",
        }
    }

    fn shows(o: &SettingsOverlay, path: &str) -> bool {
        o.visible()
            .iter()
            .any(|r| matches!(r, Row::Setting(i) if settings_schema::registry()[*i].path == path))
    }

    #[test]
    fn only_the_selected_memory_backend_shows_its_options() {
        let on_learning = |mut o: SettingsOverlay| {
            o.tab = o.tabs.iter().position(|(t, _)| *t == "Memory").unwrap();
            o
        };
        // Default provenance: no backend sections at all.
        let o = on_learning(overlay());
        assert!(shows(&o, "memory.backend") && shows(&o, "memory.autolearn"));
        for path in ["memory.hindsight.endpoint", "memory.local.extraction_role"] {
            assert!(!shows(&o, path), "{path} is noise for provenance");
        }
        let mut local = on_learning(SettingsOverlay::new(
            table("[memory]\nbackend = \"local\"\n"),
            Table::new(),
            table("[memory]\nbackend = \"local\"\n"),
            Scope::Global,
            vec![],
        ));
        assert!(shows(&local, "memory.local.extraction_role"));
        assert!(!shows(&local, "memory.hindsight.endpoint"));
        local.search = "Service URL".into();
        assert!(
            shows(&local, "memory.hindsight.endpoint"),
            "search still finds everything"
        );
        let off = on_learning(SettingsOverlay::new(
            table("[memory]\nbackend = \"off\"\n"),
            Table::new(),
            Table::new(),
            Scope::Global,
            vec![],
        ));
        assert!(shows(&off, "memory.backend") && !shows(&off, "memory.autolearn"));
    }

    #[test]
    fn choosing_a_backend_with_missing_fields_guides_through_them_before_switching() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "memory.backend");
        assert!(draw(&mut o, &rig, 110, 30).contains("Needs no setup"));
        // Pick "hindsight": nothing is saved (validation would reject it); we land on its URL.
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        let Mode::Pick {
            choices, cursor, ..
        } = &mut o.mode
        else {
            panic!()
        };
        *cursor = choices.iter().position(|c| c == "hindsight").unwrap();
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert_eq!(path_at_cursor(&o), "memory.hindsight.endpoint");
        assert!(o.feedback.as_deref().unwrap().contains("Service URL"));
        assert!(!o.pending.contains_key("memory.backend"));
        // Fill it; once saved, the cursor returns to Backend with the choice ready.
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send_all(&mut o, &rig.keys, typed("http://127.0.0.1:8888"));
        let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        o.settled(&path, Ok(value));
        assert_eq!(path_at_cursor(&o), "memory.backend");
        assert!(
            o.feedback
                .as_deref()
                .unwrap()
                .contains("Hindsight service is ready")
        );
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            (path.as_str(), value.as_str()),
            ("memory.backend", Some("hindsight"))
        );
    }

    #[test]
    fn backend_picker_starts_on_the_active_backend_after_a_guided_switch_is_abandoned() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "memory.backend");
        // Pick hindsight; its URL is missing, so the switch waits for it.
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        let Mode::Pick {
            choices, cursor, ..
        } = &mut o.mode
        else {
            panic!()
        };
        *cursor = choices.iter().position(|c| c == "hindsight").unwrap();
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send_all(&mut o, &rig.keys, typed("http://127.0.0.1:8888"));
        let (path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            (path.as_str(), scope),
            ("memory.hindsight.endpoint", Scope::Global)
        );
        o.settled(&path, Ok(value));
        assert_eq!(path_at_cursor(&o), "memory.backend");
        // Open the picker (it preselects the pending hindsight), then cancel it.
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(&mut o, &rig.keys, key(KeyCode::Esc));
        // Reopening starts on the backend that is actually active, not the abandoned choice.
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        {
            let Mode::Pick {
                choices, cursor, ..
            } = &o.mode
            else {
                panic!("picker should reopen")
            };
            assert_eq!(choices[*cursor], "provenance");
        }
        // Choosing hindsight again and confirming saves exactly that backend, globally.
        {
            let Mode::Pick {
                choices, cursor, ..
            } = &mut o.mode
            else {
                panic!("picker should still be open")
            };
            *cursor = choices.iter().position(|c| c == "hindsight").unwrap();
        }
        let (path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            (path.as_str(), &value, scope),
            (
                "memory.backend",
                &Value::String("hindsight".into()),
                Scope::Global
            )
        );
    }

    fn expect_set(event: OverlayEvent) -> (String, Value, Scope) {
        match event {
            OverlayEvent::Set { path, value, scope } => (path, value, scope),
            other => panic!("expected Set, got {other:?}"),
        }
    }

    fn choose_next(o: &mut SettingsOverlay, keys: &Keymap) -> OverlayEvent {
        assert!(matches!(
            send(o, keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(matches!(
            send(o, keys, key(KeyCode::Down)),
            OverlayEvent::None
        ));
        send(o, keys, key(KeyCode::Enter))
    }

    #[test]
    fn renders_title_tabs_sections_rows_and_footer() {
        let rig = Rig::new();
        let mut o = overlay();
        let screen = draw(&mut o, &rig, 130, 30);
        for needle in [
            "\u{25c6} Settings",
            "Save to: All projects",
            "Appearance",
            "Interaction",
            "Setup",
            "Status Line",
            "Separator",
            "Powerline thin",
            "Preview: PREVIEW",
            "Type to search",
            "Esc close",
            "Ctrl+S scope",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
        assert!(screen.starts_with('╭'), "consistent rounded frame");
    }

    #[test]
    fn row_shows_modified_marker_and_source_per_layer() {
        let rig = Rig::new();
        let mut o = SettingsOverlay::new(
            table("[ui]\nstatus_separator = \"pipe\"\ntips = false\n"),
            table("[ui]\nstatus_separator = \"pipe\"\n"),
            table("[ui]\ntips = false\n"),
            Scope::Project,
            vec![],
        );
        let screen = draw(&mut o, &rig, 120, 40);
        let line = |needle: &str| {
            screen
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle}\n{screen}"))
                .to_owned()
        };
        let sep = line("Separator");
        assert!(sep.contains("Pipe") && sep.contains("project"), "{sep}");
        let tips = line(" Tips");
        assert!(tips.contains("off") && tips.contains("global"), "{tips}");
        let welcome = line("Welcome banner");
        assert!(
            welcome.contains("on") && !welcome.contains("project") && !welcome.contains("global"),
            "{welcome}"
        );
        assert!(screen.contains("Save to: This project"));
    }

    #[test]
    fn bool_toggles_and_enum_choices_require_confirmation() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "ui.tips");
        let (path, value, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            (path.as_str(), value, scope),
            ("ui.tips", Value::Boolean(false), Scope::Global)
        );
        // Before the controller answers the displayed value stays on the pending value, then follows it.
        o.settled("ui.tips", Ok(Value::Boolean(false)));
        assert_eq!(
            settings_schema::get(&o.global, "ui.tips"),
            Some(&Value::Boolean(false)),
            "source layer updated"
        );
        let (_, value, _) = expect_set(send(&mut o, &rig.keys, ch(' ')));
        assert_eq!(value, Value::Boolean(true));

        goto(&mut o, &rig.keys, "ui.composer_shape");
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(draw(&mut o, &rig, 100, 24).contains("Line"));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Down)),
            OverlayEvent::None
        ));
        assert!(
            !o.pending.contains_key("ui.composer_shape"),
            "browsing must not save"
        );
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Esc)),
            OverlayEvent::None
        ));
        assert!(matches!(o.mode, Mode::Browse));
        let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
        assert_eq!(value, Value::String("line".into()));
        o.settled("ui.composer_shape", Ok(value));
        let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
        assert_eq!(value, Value::String("box".into()), "wraps around");
    }

    #[test]
    fn number_input_validates_locally_and_esc_cancels() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "harness.max_steps");
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(matches!(o.mode, Mode::Edit { .. }));
        // The editor starts with the current value; replace it with out-of-range input.
        send_all(
            &mut o,
            &rig.keys,
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
            ))],
        );
        send_all(&mut o, &rig.keys, typed("99"));
        assert!(
            matches!(
                send(&mut o, &rig.keys, key(KeyCode::Enter)),
                OverlayEvent::None
            ),
            "out of range is not sent"
        );
        let shown = draw(&mut o, &rig, 110, 30);
        assert!(shown.contains("Must be between 0 and 64"), "{shown}");
        assert!(matches!(o.mode, Mode::Edit { .. }), "stays in the editor");
        // Not a number at all.
        send_all(
            &mut o,
            &rig.keys,
            vec![key(KeyCode::Backspace), key(KeyCode::Backspace), ch('x')],
        );
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::None
        ));
        assert!(draw(&mut o, &rig, 110, 30).contains("Enter a whole number"));
        // A valid value goes out as an integer.
        send_all(
            &mut o,
            &rig.keys,
            vec![key(KeyCode::Backspace), ch('6'), ch('4')],
        );
        let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            (path.as_str(), value),
            ("harness.max_steps", Value::Integer(64))
        );
        assert!(matches!(o.mode, Mode::Browse));
        o.settled("harness.max_steps", Ok(Value::Integer(64)));
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(
            &mut o,
            &rig.keys,
            Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        );
        send(&mut o, &rig.keys, ch('0'));
        let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            (path.as_str(), value),
            ("harness.max_steps", Value::Integer(0))
        );
        o.settled("harness.max_steps", Ok(Value::Integer(0)));
        assert!(draw(&mut o, &rig, 110, 30).contains("0 (unlimited)"));
        assert_eq!(edit_text(&Value::Integer(0)), "0");
        // Esc leaves the editor without sending anything and without closing the panel.
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Esc)),
            OverlayEvent::None
        ));
        assert!(matches!(o.mode, Mode::Browse));
    }

    #[test]
    fn float_and_list_inputs_and_step_adjustment() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "harness.temperature");
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(&mut o, &rig.keys, key(KeyCode::Up));
        let Mode::Edit { editor, .. } = &o.mode else {
            panic!()
        };
        assert_eq!(
            editor.text, "0.3",
            "Up adds one step to 0.2 without float noise"
        );
        let (_, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(value, Value::Float(0.3));

        goto(&mut o, &rig.keys, "ui.status_left");
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        let Mode::Edit { editor, .. } = &o.mode else {
            panic!()
        };
        assert_eq!(editor.text, "spinner, model, path, git");
        send_all(
            &mut o,
            &rig.keys,
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
            ))],
        );
        send_all(&mut o, &rig.keys, typed("model , ,context"));
        let (_, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            value,
            Value::Array(vec![
                Value::String("model".into()),
                Value::String("context".into())
            ])
        );
    }

    #[test]
    fn context_editor_displays_auto_and_accepts_cloud_local_caps() {
        let rig = Rig::new();
        let mut o = overlay();
        for path in [
            "context_window",
            "cloud_context_window",
            "local_context_window",
        ] {
            goto(&mut o, &rig.keys, path);
            let screen = draw(&mut o, &rig, 130, 30);
            assert!(screen.contains("Auto"), "{screen}");
            send(&mut o, &rig.keys, key(KeyCode::Enter));
            send(&mut o, &rig.keys, ctrl('u'));
            send_all(&mut o, &rig.keys, typed("64000"));
            let (saved_path, value, scope) =
                expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
            assert_eq!(saved_path, path);
            assert_eq!(value, Value::Integer(64000));
            assert_eq!(scope, Scope::Global);
            o.settled(path, Ok(value));
            send(&mut o, &rig.keys, key(KeyCode::Enter));
            send(&mut o, &rig.keys, ctrl('u'));
            send_all(&mut o, &rig.keys, typed("AUTO"));
            let (_, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
            assert_eq!(value.as_str(), Some("auto"));
            o.settled(path, Ok(value));
        }
        let kind = settings_schema::registry()
            .iter()
            .find(|d| d.path == "context_window")
            .unwrap()
            .kind;
        assert!(parse_input(kind, "2097153").is_err());
        assert!(parse_input(kind, "2047").is_err());
        assert_eq!(adjust(kind, "2048", -1).as_deref(), Some("auto"));
        assert_eq!(adjust(kind, "auto", 1).as_deref(), Some("2048"));
    }

    #[test]
    fn delete_unsets_in_the_active_scope_only() {
        let rig = Rig::new();
        let mut o = SettingsOverlay::new(
            table("[ui]\ntips = false\n"),
            Table::new(),
            table("[ui]\ntips = false\n"),
            Scope::Project,
            vec![],
        );
        goto(&mut o, &rig.keys, "ui.tips");
        // Set in global, active scope is project: nothing to remove there, say why.
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Delete)),
            OverlayEvent::None
        ));
        assert!(draw(&mut o, &rig, 120, 30).contains("Set in the global scope"));
        send(&mut o, &rig.keys, ctrl('s'));
        match send(&mut o, &rig.keys, key(KeyCode::Delete)) {
            OverlayEvent::Unset { path, scope } => {
                assert_eq!((path.as_str(), scope), ("ui.tips", Scope::Global))
            }
            other => panic!("{other:?}"),
        }
        o.settled("ui.tips", Ok(Value::Boolean(true)));
        assert!(settings_schema::get(&o.global, "ui.tips").is_none());
        let screen = draw(&mut o, &rig, 120, 30);
        let row = screen.lines().find(|l| l.contains(" Tips")).unwrap();
        assert!(
            row.contains("on") && !row.contains("global") && !row.contains("project"),
            "{row}"
        );
        // An unmodified row has nothing to reset.
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Delete)),
            OverlayEvent::None
        ));
    }

    #[test]
    fn search_filters_across_tabs_shows_location_and_escape_clears_before_closing() {
        let rig = Rig::new();
        let mut o = overlay();
        send_all(&mut o, &rig.keys, typed("sandbox"));
        let shown = draw(&mut o, &rig, 120, 30);
        assert!(
            shown.contains("Sandbox mode") && shown.contains("Allow network"),
            "{shown}"
        );
        assert!(
            shown.contains("Tools/Sandbox"),
            "each hit names its tab and section\n{shown}"
        );
        assert!(
            !shown.contains("Welcome banner"),
            "non-matching rows are hidden"
        );
        assert!(shown.contains("Esc clear search"));
        // Hits come from other tabs than the current (Appearance) one.
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Esc)),
            OverlayEvent::None
        ));
        assert!(o.search.is_empty());
        // Appearance's Display rows sit below the fold at 30 rows with the framed chrome.
        assert!(draw(&mut o, &rig, 120, 40).contains("Welcome banner"));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Esc)),
            OverlayEvent::Close
        ));

        send_all(&mut o, &rig.keys, typed("zzzzqq"));
        assert!(draw(&mut o, &rig, 120, 30).contains("No settings match \"zzzzqq\""));
        send_all(&mut o, &rig.keys, vec![key(KeyCode::Backspace); 6]);
        assert!(o.search.is_empty());
        // Enter on a search hit acts on that hit, wherever it lives.
        send_all(&mut o, &rig.keys, typed("approval"));
        assert!(
            matches!(o.visible().first(), Some(Row::Setting(i)) if settings_schema::registry()[*i].path == "approval")
        );
        let (path, value, _) = expect_set(choose_next(&mut o, &rig.keys));
        assert_eq!(
            (path.as_str(), value),
            ("approval", Value::String("auto".into()))
        );
    }

    #[test]
    fn search_never_changes_scope_and_ctrl_s_preserves_the_query() {
        let rig = Rig::new();
        let mut o = overlay();
        send_all(&mut o, &rig.keys, typed("status"));
        assert_eq!(o.search, "status", "every letter belongs to search");
        assert_eq!(o.scope, Scope::Global);
        send(&mut o, &rig.keys, ctrl('s'));
        assert_eq!(o.scope, Scope::Project);
        assert_eq!(o.search, "status");
        assert!(draw(&mut o, &rig, 100, 24).contains("Save to: This project"));
        goto(&mut o, &rig.keys, "ui.tips");
        let (_, _, scope) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(scope, Scope::Project, "later events carry the new scope");
        send(&mut o, &rig.keys, ctrl('s'));
        assert_eq!(o.scope, Scope::Global);
    }

    #[test]
    fn settled_ok_updates_value_and_err_shows_inline_and_keeps_old_value() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "ui.status");
        let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
        assert_eq!(value, Value::String("minimal".into()));
        o.settled("ui.status", Err("status preset rejected".into()));
        let shown = draw(&mut o, &rig, 120, 30);
        assert!(shown.contains("status preset rejected"), "{shown}");
        let row = shown.lines().find(|l| l.contains("Preset")).unwrap();
        assert!(row.contains("Default"), "old value kept: {row}");
        let error_style = style_of(
            &render(120, 30, |f| {
                let p = |u: &UiConfig, w| status_preview(u, w);
                let c = DrawCtx {
                    theme: &rig.theme,
                    keys: &rig.keys,
                    status: &rig.status,
                    preview_status: &p,
                };
                o.draw(f, &c)
            }),
            "status preset rejected",
        )
        .unwrap();
        assert_eq!(
            error_style.fg,
            rig.theme.fg(Tok::Error).fg,
            "errors use the Error token"
        );
        // The next attempt clears the message; Ok then moves the value.
        let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
        assert_eq!(value, Value::String("minimal".into()));
        assert!(!draw(&mut o, &rig, 120, 30).contains("status preset rejected"));
        o.settled("ui.status", Ok(Value::String("minimal".into())));
        assert!(draw(&mut o, &rig, 120, 30).contains("Saved to global settings"));
        let row = draw(&mut o, &rig, 120, 30)
            .lines()
            .find(|l| l.contains("Preset"))
            .unwrap()
            .to_owned();
        assert!(row.contains("Minimal") && row.contains("global"), "{row}");
    }

    #[test]
    fn saved_scope_feedback_survives_controller_refresh_but_not_navigation() {
        let rig = Rig::new();
        let mut o = SettingsOverlay::new(
            table("[ui]\ntips = true"),
            table("[ui]\ntips = true"),
            Table::new(),
            Scope::Global,
            vec![],
        );
        goto(&mut o, &rig.keys, "ui.tips");
        expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert!(!draw(&mut o, &rig, 100, 24).contains("Saved globally"));
        o.settled("ui.tips", Ok(Value::Boolean(true)));
        o.replace_values(o.effective.clone(), o.project.clone(), o.global.clone());
        assert!(
            draw(&mut o, &rig, 100, 24)
                .contains("Saved globally; this project's override still applies")
        );
        send(&mut o, &rig.keys, key(KeyCode::Down));
        assert!(!draw(&mut o, &rig, 100, 24).contains("Saved globally"));
    }

    #[test]
    fn short_panels_prioritize_search_and_non_appearance_settings_have_no_preview() {
        let rig = Rig::new();
        let mut o = overlay();
        let shown = draw(&mut o, &rig, 60, 8);
        assert!(shown.contains("Type to search all settings"), "{shown}");
        assert!(
            shown.contains("Enter choose") && shown.contains("Esc close"),
            "{shown}"
        );
        goto(&mut o, &rig.keys, "sandbox.mode");
        assert!(!draw(&mut o, &rig, 100, 24).contains("Preview:"));
    }

    #[test]
    fn compact_choice_picker_keeps_the_highlighted_option_visible() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "ui.theme");
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        for height in [6, 8, 10, 12] {
            assert!(
                draw(&mut o, &rig, 50, height).contains("● auto"),
                "highlight hidden at height {height}"
            );
        }
        send(&mut o, &rig.keys, key(KeyCode::PageDown));
        for height in [6, 8, 10, 12] {
            assert!(
                draw(&mut o, &rig, 50, height).contains("● auto"),
                "highlight hidden at height {height}"
            );
        }
    }

    #[test]
    fn preview_row_receives_the_pending_config() {
        let rig = Rig::new();
        let mut o = overlay();
        let seen: RefCell<Vec<UiConfig>> = RefCell::new(Vec::new());
        let preview = |ui: &UiConfig, _w: u16| {
            seen.borrow_mut().push(ui.clone());
            Line::from(format!("sep={:?}", ui.status_separator))
        };
        let paint = |o: &mut SettingsOverlay| {
            let ctx = DrawCtx {
                theme: &rig.theme,
                keys: &rig.keys,
                status: &rig.status,
                preview_status: &preview,
            };
            screen(&render(120, 30, |f| o.draw(f, &ctx)))
        };
        assert!(paint(&mut o).contains("sep=PowerlineThin"));
        goto(&mut o, &rig.keys, "ui.status_separator");
        let (_, value, _) = expect_set(choose_next(&mut o, &rig.keys));
        assert_eq!(
            value,
            Value::String("slash".into()),
            "next after powerline-thin"
        );
        let shown = paint(&mut o);
        assert!(
            shown.contains("Preview: sep=Slash"),
            "pending value is previewed before the controller answers\n{shown}"
        );
        o.settled("ui.status_separator", Err("nope".into()));
        assert!(
            paint(&mut o).contains("sep=PowerlineThin"),
            "a rejected change leaves the preview"
        );
        assert!(seen.borrow().len() >= 3);
    }

    #[test]
    fn invalid_ui_table_falls_back_to_default_preview() {
        let rig = Rig::new();
        let mut o = SettingsOverlay::new(
            table("[ui]\nbogus = 1\n"),
            Table::new(),
            Table::new(),
            Scope::Global,
            vec![],
        );
        let seen: RefCell<Option<UiConfig>> = RefCell::new(None);
        let preview = |ui: &UiConfig, _w: u16| {
            *seen.borrow_mut() = Some(ui.clone());
            Line::from("x")
        };
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &preview,
        };
        let _ = render(100, 24, |f| o.draw(f, &ctx));
        assert_eq!(
            seen.borrow().as_ref().unwrap().status_separator,
            UiConfig::default().status_separator
        );
    }

    #[test]
    fn theme_rows_pick_with_live_preview_and_cancel_restores() {
        let rig = Rig::new();
        // The picker starts on the active theme; pin it to `dark` (the default is now `auto`).
        let mut o = SettingsOverlay::new(
            table("[ui]\ntheme = \"dark\"\n"),
            Table::new(),
            Table::new(),
            Scope::Global,
            vec!["dark".into(), "light".into(), "mono".into()],
        );
        goto(&mut o, &rig.keys, "ui.theme");
        assert!(
            matches!(
                send(&mut o, &rig.keys, key(KeyCode::Enter)),
                OverlayEvent::None
            ),
            "opening the picker emits nothing"
        );
        let shown = draw(&mut o, &rig, 120, 30);
        assert!(
            shown.contains("○ light") && shown.contains("○ mono") && shown.contains("○ auto"),
            "{shown}"
        );
        match send(&mut o, &rig.keys, key(KeyCode::Down)) {
            OverlayEvent::PreviewTheme(Some(name)) => assert_eq!(name, "light"),
            other => panic!("{other:?}"),
        }
        match send(&mut o, &rig.keys, key(KeyCode::Down)) {
            OverlayEvent::PreviewTheme(Some(name)) => assert_eq!(name, "mono"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Esc)),
            OverlayEvent::PreviewTheme(None)
        ));
        assert!(matches!(o.mode, Mode::Browse), "Esc closes only the picker");

        send(&mut o, &rig.keys, key(KeyCode::Enter));
        send(&mut o, &rig.keys, key(KeyCode::Down));
        let (path, value, _) = expect_set(send(&mut o, &rig.keys, key(KeyCode::Enter)));
        assert_eq!(
            (path.as_str(), value),
            ("ui.theme", Value::String("light".into()))
        );
        // Confirming the theme that is already active just ends the preview.
        o.settled("ui.theme", Ok(Value::String("light".into())));
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        assert!(matches!(
            send(&mut o, &rig.keys, key(KeyCode::Enter)),
            OverlayEvent::PreviewTheme(None)
        ));
        // `auto` is only offered for the main theme, not for the dark/light slots.
        goto(&mut o, &rig.keys, "ui.theme_dark");
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        let shown = draw(&mut o, &rig, 120, 30);
        assert!(
            shown.contains("○ mono") && !shown.contains("○ auto") && !shown.contains("● auto"),
            "{shown}"
        );
    }

    #[test]
    fn tabs_and_sections_follow_the_keymap_including_remapped_keys() {
        let rig = Rig::new();
        let mut o = overlay();
        let tab_name =
            |o: &SettingsOverlay| o.tabs.get(o.tab).map(|(t, _)| *t).unwrap_or(PROVIDERS_TAB);
        send(&mut o, &rig.keys, key(KeyCode::Right));
        assert_eq!(tab_name(&o), "Interaction");
        send(&mut o, &rig.keys, key(KeyCode::Left));
        send(&mut o, &rig.keys, key(KeyCode::Left));
        assert_eq!(tab_name(&o), PROVIDERS_TAB, "wraps to the last tab");
        send(&mut o, &rig.keys, key(KeyCode::Right));
        assert_eq!(tab_name(&o), "Appearance");

        // Tab / Shift+Tab jump between sections of the tab.
        let section = |o: &SettingsOverlay| SettingsOverlay::section_of(o.current_row().unwrap());
        assert_eq!(section(&o), "Theme");
        send(&mut o, &rig.keys, key(KeyCode::Tab));
        assert_eq!(section(&o), "Symbols");
        send(&mut o, &rig.keys, key(KeyCode::Tab));
        assert_eq!(section(&o), "Composer");
        send(
            &mut o,
            &rig.keys,
            Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        );
        assert_eq!(section(&o), "Symbols");
        for _ in 0..5 {
            send(&mut o, &rig.keys, key(KeyCode::Tab));
        }
        assert_eq!(section(&o), "Symbols", "wraps around the sections");

        // Remapped keymap: Ctrl+L / Ctrl+H switch tabs, Ctrl+J / Ctrl+K move, Ctrl+O confirms. Old keys do nothing.
        let mut remapped = Keymap::defaults();
        for (action, chord) in [
            (Action::TabNext, "Ctrl+L"),
            (Action::TabPrev, "Ctrl+H"),
            (Action::SelectDown, "Ctrl+J"),
            (Action::SelectUp, "Ctrl+K"),
            (Action::SelectConfirm, "Ctrl+O"),
            (Action::SectionNext, "Ctrl+N"),
        ] {
            remapped.rebind(action, &[chord]).unwrap();
        }
        let mut o = overlay();
        send(&mut o, &remapped, key(KeyCode::Right));
        assert_eq!(tab_name(&o), "Appearance", "default chord no longer bound");
        send(&mut o, &remapped, ctrl('l'));
        assert_eq!(tab_name(&o), "Interaction");
        send(&mut o, &remapped, ctrl('h'));
        assert_eq!(tab_name(&o), "Appearance");
        send(&mut o, &remapped, ctrl('j'));
        assert_eq!(o.cursor, 1);
        send(&mut o, &remapped, ctrl('k'));
        assert_eq!(o.cursor, 0);
        send(&mut o, &remapped, ctrl('n'));
        assert_eq!(section(&o), "Symbols");
        let hints = draw(
            &mut o,
            &Rig {
                keys: remapped.clone(),
                ..Rig::new()
            },
            140,
            30,
        );
        assert!(
            hints.contains("Ctrl+K/Ctrl+J move") && hints.contains("Ctrl+N sections"),
            "footer reflects the remap\n{hints}"
        );
        assert!(matches!(
            send(&mut o, &remapped, ctrl('o')),
            OverlayEvent::None
        ));
        assert!(matches!(
            send(&mut o, &remapped, ctrl('j')),
            OverlayEvent::None
        ));
        match send(&mut o, &remapped, ctrl('o')) {
            OverlayEvent::Set { path, .. } => assert_eq!(path, "ui.symbols"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn providers_tab_opens_the_other_screens() {
        let rig = Rig::new();
        let mut o = overlay();
        send(&mut o, &rig.keys, key(KeyCode::Left));
        let shown = draw(&mut o, &rig, 110, 24);
        assert!(
            shown.contains("Providers & API keys")
                && shown.contains("Models & roles")
                && shown.contains("Keyboard shortcuts"),
            "{shown}"
        );
        match send(&mut o, &rig.keys, key(KeyCode::Enter)) {
            OverlayEvent::Action(a) => assert_eq!(a, "open:providers"),
            other => panic!("{other:?}"),
        }
        send(&mut o, &rig.keys, key(KeyCode::Down));
        match send(&mut o, &rig.keys, key(KeyCode::Enter)) {
            OverlayEvent::Action(a) => assert_eq!(a, "open:models"),
            other => panic!("{other:?}"),
        }
        send(&mut o, &rig.keys, key(KeyCode::Down));
        match send(&mut o, &rig.keys, key(KeyCode::Enter)) {
            OverlayEvent::Action(a) => assert_eq!(a, "open:shortcuts"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn restart_required_rows_are_marked() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "sandbox.mode");
        let shown = draw(&mut o, &rig, 130, 30);
        assert!(
            shown
                .lines()
                .find(|l| l.contains("Sandbox mode"))
                .unwrap()
                .contains("restart"),
            "{shown}"
        );
    }

    #[test]
    fn long_lists_scroll_to_keep_the_cursor_visible_and_show_a_scrollbar() {
        let rig = Rig::new();
        // The local backend shows its own long list (`memory.local.*`); provenance shows none.
        let mut o = SettingsOverlay::new(
            table("[memory]\nbackend = \"local\"\n"),
            Table::new(),
            table("[memory]\nbackend = \"local\"\n"),
            Scope::Global,
            vec![],
        );
        o.tab = o.tabs.iter().position(|(t, _)| *t == "Memory").unwrap();
        o.advanced_open.insert(("Memory", "Rollout memory"));
        let shown = draw(&mut o, &rig, 120, 16);
        assert!(shown.contains(rig.theme.sym(Sym::ScrollThumb)), "{shown}");
        for _ in 0..20 {
            send(&mut o, &rig.keys, key(KeyCode::Down));
        }
        let shown = draw(&mut o, &rig, 120, 16);
        let label = match o.current_row() {
            Some(Row::Setting(i)) => settings_schema::registry()[i].label.to_owned(),
            Some(Row::Advanced(..)) => "advanced (".to_owned(),
            _ => panic!("cursor is on a row"),
        };
        assert!(
            shown.contains(&label),
            "cursor row {label:?} visible\n{shown}"
        );
        send(&mut o, &rig.keys, key(KeyCode::PageDown));
        assert!(o.cursor > 20);
        send(&mut o, &rig.keys, key(KeyCode::Up));
    }

    #[test]
    fn replace_values_drops_pending_state() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "ui.tips");
        send(&mut o, &rig.keys, key(KeyCode::Enter));
        o.settled("ui.tips", Err("x".into()));
        o.replace_values(table("[ui]\ntips = false\n"), Table::new(), Table::new());
        assert!(o.pending.is_empty() && o.notes.is_empty());
        assert!(
            draw(&mut o, &rig, 120, 30)
                .lines()
                .find(|l| l.contains(" Tips"))
                .unwrap()
                .contains("off")
        );
    }

    #[test]
    fn tiny_terminals_never_panic_in_any_mode() {
        // Each width is an independent slice of the grid; threads keep the 961-cell sweep short
        // while every (width, height) pair still runs the same sequence.
        std::thread::scope(|scope| {
            for w in 0..=30u16 {
                scope.spawn(move || {
                    let rig = Rig::new();
                    for h in 0..=30u16 {
                        let mut o = overlay();
                        draw(&mut o, &rig, w, h);
                        goto(&mut o, &rig.keys, "harness.temperature");
                        send(&mut o, &rig.keys, key(KeyCode::Enter));
                        draw(&mut o, &rig, w, h);
                        send(&mut o, &rig.keys, key(KeyCode::Esc));
                        goto(&mut o, &rig.keys, "ui.theme");
                        send(&mut o, &rig.keys, key(KeyCode::Enter));
                        draw(&mut o, &rig, w, h);
                        send(&mut o, &rig.keys, key(KeyCode::Esc));
                        send_all(&mut o, &rig.keys, typed("ma"));
                        draw(&mut o, &rig, w, h);
                        send_all(&mut o, &rig.keys, typed("zzzz"));
                        draw(&mut o, &rig, w, h);
                    }
                });
            }
        });
    }

    #[test]
    fn wide_and_hostile_text_is_kept_inside_the_frame() {
        let rig = Rig::new();
        let mut o = SettingsOverlay::new(
            table("[ui]\nstatus_left = [\"日本語日本語日本語日本語\", \"\\u001b[31mred\"]\n"),
            Table::new(),
            Table::new(),
            Scope::Global,
            vec![],
        );
        goto(&mut o, &rig.keys, "ui.status_left");
        let shown = draw(&mut o, &rig, 60, 20);
        assert!(!shown.contains('\u{1b}'));
        for line in shown.lines() {
            assert!(text::width(line) <= 80);
        }
    }
    #[test]
    fn experimental_tab_renders_each_new_toggle_off_by_default() {
        let rig = Rig::new();
        let mut o = overlay();
        goto(&mut o, &rig.keys, "experimental.work_packet_contracts");
        let shown = draw(&mut o, &rig, 120, 32);
        for label in [
            "Work packet contracts",
            "Verification receipts",
            "Overlap deduplication",
            "Profile matching",
        ] {
            assert!(shown.contains(label), "missing {label} in:\n{shown}");
        }
        assert!(shown.contains("Experimental"));
        assert!(
            shown.contains("off"),
            "toggles should render disabled:\n{shown}"
        );
    }
}

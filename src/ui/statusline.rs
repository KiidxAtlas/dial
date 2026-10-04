//! The status line: named segments (model, path, git, context, …) laid out into the composer's top
//! border. Everything is derived from [`StatusData`] (unknown facts are absent, never `0`), styled
//! from theme tokens and glyphs, and fitted to the exact width: when space runs out the least
//! important segments are dropped first, then the long texts are shortened with an ellipsis. The
//! line never wraps and is always exactly `width` cells.
use ratatui::{style::Style, text::{Line, Span}};

use crate::llm::model_config::{StatusPreset, StatusSeparator, UiConfig};

use super::{status::StatusData, text, theme::{Glyphs, Sym, Theme, Tok}};

/// Segments below this importance are dropped before any text is shortened.
const SHRINK_BEFORE_DROPPING: u8 = 60;
const MODEL_FLOOR: usize = 10;
const PATH_FLOOR: usize = 6;
const LAST_RESORT_FLOOR: usize = 2;
const CONTEXT_BAR_CELLS: usize = 4;
const CONTEXT_WARN: f64 = 0.60;
const CONTEXT_CRITICAL: f64 = 0.85;

/// Segment names for the configured preset: `(left, right)`. `custom` uses `ui.status_left/right`
/// verbatim (unknown names are skipped when rendering).
pub fn segments(cfg: &UiConfig) -> (Vec<String>, Vec<String>) {
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    match cfg.status {
        StatusPreset::Default | StatusPreset::Nerd | StatusPreset::Ascii => (names(&["spinner", "model", "path", "git"]), names(&["context"])),
        StatusPreset::Minimal => (names(&["model"]), names(&["context"])),
        StatusPreset::Compact => (names(&["spinner", "model", "git"]), names(&["context", "tokens"])),
        StatusPreset::Full => (
            names(&["spinner", "model", "role", "path", "git", "approval", "sandbox"]),
            names(&["tokens", "rate", "learning", "context"]),
        ),
        StatusPreset::Custom => (cfg.status_left.clone(), cfg.status_right.clone()),
    }
}

/// The composer's top border with the status segments embedded: `╭─ left ──── right ─╮`, exactly
/// `width` cells. `border` styles the rule; segments use their own tokens.
pub fn border_line(cfg: &UiConfig, data: &StatusData, theme: &Theme, width: u16, border: Style, left_corner: &str, right_corner: &str) -> Line<'static> {
    compose(cfg, data, theme, width as usize, border, left_corner, right_corner)
}

/// The same line without corners, for the settings preview.
pub fn preview_row(cfg: &UiConfig, data: &StatusData, theme: &Theme, width: u16) -> Line<'static> {
    compose(cfg, data, theme, width as usize, theme.fg(Tok::Border), "", "")
}

// ---------------------------------------------------------------------------------------------
// Segments
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shrink {
    /// Cut the end (`mlx/qwen3-9…`).
    Tail,
    /// Cut the start, keeping the informative tail (`…ects/dial`).
    Head,
}

#[derive(Clone)]
struct Part {
    text: String,
    style: Style,
    shrink: Option<Shrink>,
}

#[derive(Clone)]
struct Seg {
    name: &'static str,
    importance: u8,
    parts: Vec<Part>,
}

impl Seg {
    fn width(&self) -> usize { self.parts.iter().map(|p| text::width(&p.text)).sum() }

    /// Shorten the shrinkable part by up to `by` cells without going under `floor` cells.
    fn shrink(&mut self, by: usize, floor: usize, ascii: bool) {
        let Some(part) = self.parts.iter_mut().find(|p| p.shrink.is_some()) else { return };
        let current = text::width(&part.text);
        let target = floor.max(current.saturating_sub(by));
        if target >= current { return; }
        part.text = match part.shrink {
            Some(Shrink::Head) => ellipsize_start(&part.text, target, ascii),
            _ => text::ellipsize(&part.text, target, ascii),
        };
    }
}

fn ellipsize_start(value: &str, max: usize, ascii: bool) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    if text::width(value) <= max { return value.to_owned(); }
    let mark = if ascii { "..." } else { "…" };
    let room = max.saturating_sub(text::width(mark));
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for grapheme in value.graphemes(true).rev() {
        used += text::width(grapheme);
        if used > room { break; }
        kept.push(grapheme);
    }
    kept.reverse();
    format!("{mark}{}", kept.concat())
}

fn canonical(name: &str) -> Option<(&'static str, u8)> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "model" => ("model", 100),
        "context" => ("context", 90),
        "spinner" => ("spinner", 80),
        "path" => ("path", 60),
        "role" => ("role", 55),
        "git" => ("git", 50),
        "approval" => ("approval", 45),
        "sandbox" => ("sandbox", 44),
        "tokens" => ("tokens", 40),
        "rate" => ("rate", 35),
        "learning" => ("learning", 30),
        "session" => ("session", 20),
        "time" => ("time", 20),
        _ => return None,
    })
}

struct Env<'a> {
    cfg: &'a UiConfig,
    data: &'a StatusData,
    theme: &'a Theme,
    /// Background of the status groups (`StatusBg`), empty when transparent.
    base: Style,
    home: Option<String>,
}

impl Env<'_> {
    fn style(&self, tok: Tok) -> Style { self.theme.fg(tok).patch(self.base) }
    fn part(&self, value: impl Into<String>, tok: Tok) -> Part { Part { text: value.into(), style: self.style(tok), shrink: None } }
    fn shrinkable(&self, value: impl Into<String>, tok: Tok, how: Shrink) -> Part {
        Part { shrink: Some(how), ..self.part(value, tok) }
    }
    /// The icon plus a space, or `fallback` plus a space when the symbol set has no icon (ASCII).
    fn icon(&self, sym: Sym, tok: Tok, fallback: &str) -> Option<Part> {
        let glyph = self.theme.sym(sym);
        let label = if glyph.is_empty() { fallback } else { glyph };
        (!label.is_empty()).then(|| self.part(format!("{label} "), tok))
    }
    fn seg(&self, name: &'static str, importance: u8, parts: Vec<Option<Part>>) -> Option<Seg> {
        Some(Seg { name, importance, parts: parts.into_iter().flatten().collect() })
    }

    fn build(&self, name: &str) -> Option<Seg> {
        let (name, importance) = canonical(name)?;
        let data = self.data;
        match name {
            "spinner" => {
                if !data.busy() { return None; }
                let frame = self.theme.spinner(data.tick);
                let activity = text::one_line(&data.activity);
                let mut label = frame.to_owned();
                if !activity.trim().is_empty() { label.push_str(&format!(" {}", activity.trim())); }
                if data.elapsed_secs.is_finite() && data.elapsed_secs >= 0.0 {
                    label.push_str(&format!(" {}", elapsed(data.elapsed_secs)));
                }
                self.seg(name, importance, vec![Some(self.part(label, Tok::Success))])
            }
            "model" => {
                let model = text::one_line(&data.model);
                let model = model.trim();
                if model.is_empty() { return None; }
                let provider = text::one_line(&data.provider);
                let provider = provider.trim();
                let short = short_model(model);
                let label = if provider.is_empty() { short } else { format!("{provider}/{short}") };
                self.seg(name, importance, vec![
                    self.icon(Sym::IconModel, Tok::StatusModel, ""),
                    Some(self.shrinkable(label, Tok::StatusModel, Shrink::Tail)),
                ])
            }
            "role" => {
                let role = text::one_line(&data.role);
                let role = role.trim();
                if role.is_empty() || role == "main" { return None; }
                self.seg(name, importance, vec![Some(self.part(role, Tok::Accent))])
            }
            "path" => {
                let path = text::one_line(&data.project_path);
                if path.trim().is_empty() { return None; }
                let shown = tilde(path.trim(), self.home.as_deref());
                let shown = if self.cfg.status_path_abbreviate { last_components(&shown, 2) } else { shown };
                self.seg(name, importance, vec![
                    self.icon(Sym::IconFolder, Tok::StatusPath, ""),
                    Some(self.shrinkable(shown, Tok::StatusPath, Shrink::Head)),
                ])
            }
            "git" => {
                let branch = text::one_line(&data.branch);
                let branch = branch.trim();
                if branch.is_empty() { return None; }
                let counts = data.git_counts.filter(|_| self.cfg.status_git_counts);
                let dirty = data.git_counts.is_some_and(|(s, m, u)| s + m + u > 0);
                let tok = if dirty { Tok::StatusGitDirty } else { Tok::StatusGitClean };
                let counts = counts.map(|(s, m, u)| {
                    [(s, '+'), (m, '~'), (u, '?')].iter().filter(|(n, _)| *n > 0).map(|(n, c)| format!("{c}{n}")).collect::<Vec<_>>().join(" ")
                }).filter(|c| !c.is_empty());
                self.seg(name, importance, vec![
                    self.icon(Sym::IconGit, tok, ""),
                    Some(self.shrinkable(branch, tok, Shrink::Tail)),
                    counts.map(|c| self.part(format!(" {c}"), tok)),
                ])
            }
            "context" => Some(self.context(importance)),
            "tokens" => {
                if data.output_tokens == 0 { return None; }
                let label = format!("{}{}", self.theme.sym(Sym::ArrowDown), human(data.output_tokens));
                self.seg(name, importance, vec![self.icon(Sym::IconTokens, Tok::StatusTokens, ""), Some(self.part(label, Tok::StatusTokens))])
            }
            "rate" => {
                let rate = data.tokens_per_second.filter(|r| r.is_finite() && *r > 0.0)?;
                self.seg(name, importance, vec![self.icon(Sym::IconRate, Tok::StatusRate, ""), Some(self.part(format!("{rate:.1} tok/s"), Tok::StatusRate))])
            }
            "approval" => {
                let value = text::one_line(&data.approval);
                if value.trim().is_empty() { return None; }
                self.seg(name, importance, vec![self.icon(Sym::IconApproval, Tok::StatusApproval, "approval"), Some(self.part(value.trim(), Tok::StatusApproval))])
            }
            "sandbox" => {
                let value = text::one_line(&data.sandbox);
                if value.trim().is_empty() { return None; }
                self.seg(name, importance, vec![self.icon(Sym::IconSandbox, Tok::StatusSandbox, "sandbox"), Some(self.part(value.trim(), Tok::StatusSandbox))])
            }
            "learning" => {
                let state = text::one_line(&data.learning);
                let mut pieces = Vec::new();
                if let Some(version) = data.version { pieces.push(format!("v{version}")); }
                if !state.trim().is_empty() { pieces.push(format!("learning {}", state.trim())); }
                if pieces.is_empty() { return None; }
                let joiner = format!(" {} ", self.theme.sym(Sym::Dot));
                self.seg(name, importance, vec![self.icon(Sym::IconLearning, Tok::StatusLearning, ""), Some(self.part(pieces.join(&joiner), Tok::StatusLearning))])
            }
            "session" => {
                let id = text::one_line(&data.session);
                let id = text::clip(id.trim(), 8);
                if id.is_empty() { return None; }
                self.seg(name, importance, vec![self.icon(Sym::IconSession, Tok::StatusSession, ""), Some(self.part(id, Tok::StatusSession))])
            }
            "time" => {
                let clock = text::one_line(&data.clock);
                if clock.trim().is_empty() { return None; }
                self.seg(name, importance, vec![self.icon(Sym::IconClock, Tok::StatusTime, ""), Some(self.part(clock.trim(), Tok::StatusTime))])
            }
            _ => None,
        }
    }

    /// `◔ 12% ██·· 3.9k/32k`, or `◔ ctx ?` when the prompt size or the window is unknown.
    fn context(&self, importance: u8) -> Seg {
        let icon = self.icon(Sym::IconContext, Tok::StatusContext, "");
        let data = self.data;
        let (Some(fraction), Some(used)) = (data.context_fraction(), data.prompt_tokens) else {
            return Seg { name: "context", importance, parts: icon.into_iter().chain([self.part("ctx ?", Tok::StatusContext)]).collect() };
        };
        let tok = if fraction >= CONTEXT_CRITICAL { Tok::Error } else if fraction >= CONTEXT_WARN { Tok::Warning } else { Tok::Success };
        let filled = ((fraction * CONTEXT_BAR_CELLS as f64).round() as usize).max(usize::from(fraction > 0.0)).min(CONTEXT_BAR_CELLS);
        let bar_on = self.theme.sym(Sym::ScrollThumb).repeat(filled);
        let bar_off = self.theme.sym(Sym::Dot).repeat(CONTEXT_BAR_CELLS - filled);
        let mut parts: Vec<Part> = icon.into_iter().collect();
        parts.push(self.part(format!("{}%", (fraction * 100.0).round() as u32), tok));
        parts.push(self.part(" ", Tok::StatusContext));
        parts.push(self.part(bar_on, tok));
        parts.push(self.part(bar_off, Tok::StatusSep));
        parts.push(self.part(format!(" {}/{}", human(used), human(u64::from(data.context_window))), Tok::StatusContext));
        Seg { name: "context", importance, parts }
    }
}

/// `12s`, `1m05s`, `1h02m`.
fn elapsed(secs: f64) -> String {
    let total = secs as u64;
    match total {
        0..=59 => format!("{total}s"),
        60..=3599 => format!("{}m{:02}s", total / 60, total % 60),
        _ => format!("{}h{:02}m", total / 3600, total % 3600 / 60),
    }
}

/// `3.9k`, `32k`, `1.2M`: floored so the used amount never looks larger than it is.
fn human(n: u64) -> String {
    let scaled = |unit: u64, suffix: &str| {
        let tenths = n * 10 / unit;
        if tenths % 10 == 0 { format!("{}{suffix}", tenths / 10) } else { format!("{}.{}{suffix}", tenths / 10, tenths % 10) }
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => scaled(1_000, "k"),
        10_000..=999_999 => format!("{}k", n / 1_000),
        1_000_000..=9_999_999 => scaled(1_000_000, "M"),
        _ => format!("{}M", n / 1_000_000),
    }
}

/// `models--org--name` cache directories and absolute paths shortened to the model's own name.
fn short_model(model: &str) -> String {
    let path = std::path::Path::new(model);
    if let Some(name) = path.components().find_map(|part| part.as_os_str().to_str().and_then(|s| s.strip_prefix("models--"))) {
        return name.replace("--", "/");
    }
    if path.is_absolute() {
        if let Some(name) = path.file_name() { return name.to_string_lossy().into_owned(); }
    }
    model.to_owned()
}

fn tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.map(|h| h.trim_end_matches('/')).filter(|h| !h.is_empty()) else { return path.to_owned() };
    match path.strip_prefix(home) {
        Some("") => "~".to_owned(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

fn last_components(path: &str, keep: usize) -> String {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.len() <= keep {
        return if path.len() > 1 { path.trim_end_matches('/').to_owned() } else { path.to_owned() };
    }
    components[components.len() - keep..].join("/")
}

// ---------------------------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------------------------

fn group_width(segs: &[Seg], sep: usize) -> usize {
    if segs.is_empty() { 0 } else { segs.iter().map(Seg::width).sum::<usize>() + sep * (segs.len() - 1) }
}

/// Cells needed by both groups, including their one-space padding and the minimum rule between them.
fn needed(left: &[Seg], right: &[Seg], sep: usize) -> usize {
    let (lw, rw) = (group_width(left, sep), group_width(right, sep));
    let padded = |w: usize| if w == 0 { 0 } else { w + 2 };
    padded(lw) + padded(rw) + usize::from(lw > 0 && rw > 0)
}

/// Remove the least important unprotected segment below `below` (ties: rightmost of the left group
/// first, then the right group). False when nothing was removable.
fn drop_least_important(left: &mut Vec<Seg>, right: &mut Vec<Seg>, protect_model: bool, below: u8) -> bool {
    let candidates = left.iter().enumerate().rev().map(|(i, s)| (false, i, s))
        .chain(right.iter().enumerate().rev().map(|(i, s)| (true, i, s)));
    let mut best: Option<(bool, usize, u8)> = None;
    for (in_right, index, seg) in candidates {
        if seg.importance >= below || (protect_model && seg.name == "model") { continue; }
        if best.is_none_or(|(_, _, importance)| seg.importance < importance) { best = Some((in_right, index, seg.importance)); }
    }
    let Some((in_right, index, _)) = best else { return false };
    if in_right { right.remove(index); } else { left.remove(index); }
    true
}

fn shrink_texts(left: &mut [Seg], right: &mut [Seg], avail: usize, sep: usize, ascii: bool, floors: &[(&str, usize)]) {
    for (name, floor) in floors {
        let over = needed(left, right, sep).saturating_sub(avail);
        if over == 0 { return; }
        if let Some(seg) = left.iter_mut().chain(right.iter_mut()).find(|s| s.name == *name) { seg.shrink(over, *floor, ascii); }
    }
}

/// Make the groups fit `avail` cells: drop the minor segments, shorten path then model, drop the
/// rest by importance (the model last), and only then squeeze the model below its comfortable size.
fn fit(left: &mut Vec<Seg>, right: &mut Vec<Seg>, avail: usize, sep: usize, ascii: bool) {
    while needed(left, right, sep) > avail && drop_least_important(left, right, false, SHRINK_BEFORE_DROPPING) {}
    shrink_texts(left, right, avail, sep, ascii, &[("path", PATH_FLOOR), ("model", MODEL_FLOOR)]);
    while needed(left, right, sep) > avail && drop_least_important(left, right, true, u8::MAX) {}
    shrink_texts(left, right, avail, sep, ascii, &[("model", LAST_RESORT_FLOOR)]);
    if needed(left, right, sep) > avail {
        // Without room for the icon the model name still beats nothing.
        for seg in left.iter_mut().chain(right.iter_mut()) {
            if seg.name == "model" && seg.parts.len() > 1 && seg.parts[0].shrink.is_none() { seg.parts.remove(0); }
        }
        shrink_texts(left, right, avail, sep, ascii, &[("model", LAST_RESORT_FLOOR)]);
    }
    if needed(left, right, sep) > avail { left.clear(); right.clear(); }
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn separator_glyph(cfg: &UiConfig, theme: &Theme) -> String {
    match cfg.status_separator {
        StatusSeparator::Powerline => theme.sym(Sym::SepPowerline).to_owned(),
        StatusSeparator::PowerlineThin => theme.sym(Sym::SepPowerlineThin).to_owned(),
        StatusSeparator::Slash => theme.sym(Sym::SepSlash).to_owned(),
        StatusSeparator::Pipe => theme.sym(Sym::SepPipe).to_owned(),
        StatusSeparator::Block => theme.sym(Sym::SepBlock).to_owned(),
        StatusSeparator::None => String::new(),
        // The ASCII style is ASCII whatever the active symbol set is.
        StatusSeparator::Ascii => theme.clone().with_glyphs(Glyphs::Ascii).sym(Sym::SepPowerlineThin).to_owned(),
    }
}

/// A plain rule of exactly `width` cells with the corners when they fit.
fn rule(width: usize, left: &str, right: &str, bar: &str, border: Style) -> Line<'static> {
    let corners = text::width(left) + text::width(right);
    let spans = if width >= corners {
        vec![Span::styled(format!("{left}{}{right}", bar.repeat(width - corners)), border)]
    } else {
        vec![Span::styled(bar.repeat(width), border)]
    };
    Line::from(spans)
}

fn group_spans(segs: &[Seg], sep_spans: &[Span<'static>]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, seg) in segs.iter().enumerate() {
        if index > 0 { spans.extend(sep_spans.iter().cloned()); }
        spans.extend(seg.parts.iter().map(|p| Span::styled(p.text.clone(), p.style)));
    }
    spans
}

fn compose(cfg: &UiConfig, data: &StatusData, theme: &Theme, width: usize, border: Style, left_corner: &str, right_corner: &str) -> Line<'static> {
    let bar = theme.sym(Sym::BoxH);
    let frame = text::width(left_corner) + text::width(right_corner) + 2 * text::width(bar);
    if width < frame { return rule(width, left_corner, right_corner, bar, border); }

    let base = if cfg.status_transparent { Style::default() } else { theme.bg(Tok::StatusBg) };
    let env = Env { cfg, data, theme, base, home: std::env::var("HOME").ok() };
    let (left_names, right_names) = segments(cfg);
    let build = |names: &[String]| names.iter().filter_map(|n| env.build(n)).collect::<Vec<_>>();
    let (mut left, mut right) = (build(&left_names), build(&right_names));

    let glyph = separator_glyph(cfg, theme);
    let sep_text = if glyph.is_empty() { "  ".to_owned() } else { format!(" {glyph} ") };
    let sep_width = text::width(&sep_text);
    let sep_spans = if glyph.is_empty() {
        vec![Span::styled(sep_text, base)]
    } else {
        vec![Span::styled(" ", base), Span::styled(glyph, theme.fg(Tok::StatusSep).patch(base)), Span::styled(" ", base)]
    };

    let avail = width - frame;
    fit(&mut left, &mut right, avail, sep_width, theme.ascii());
    if left.is_empty() && right.is_empty() { return rule(width, left_corner, right_corner, bar, border); }

    let pad = |segs: &[Seg]| if segs.is_empty() { 0 } else { group_width(segs, sep_width) + 2 };
    let fill = avail - pad(&left) - pad(&right);
    let padding = || Span::styled(" ", base);
    let mut spans = vec![Span::styled(format!("{left_corner}{bar}"), border)];
    if !left.is_empty() {
        spans.push(padding());
        spans.extend(group_spans(&left, &sep_spans));
        spans.push(padding());
    }
    spans.push(Span::styled(bar.repeat(fill), border));
    if !right.is_empty() {
        spans.push(padding());
        spans.extend(group_spans(&right, &sep_spans));
        spans.push(padding());
    }
    spans.push(Span::styled(format!("{bar}{right_corner}"), border));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::status::Phase;
    use ratatui::style::Color;

    fn data() -> StatusData {
        StatusData {
            provider: "mlx".into(), model: "qwen3-9b".into(), project_path: "/work/projects/dial".into(),
            branch: "main".into(), git_counts: Some((1, 2, 3)), prompt_tokens: Some(3900), context_window: 32_000,
            ..StatusData::default()
        }
    }
    fn plain(line: &Line) -> String { line.spans.iter().map(|s| s.content.as_ref()).collect() }
    fn render(cfg: &UiConfig, data: &StatusData, width: u16) -> Line<'static> {
        border_line(cfg, data, &Theme::dark(), width, Style::default(), "╭", "╮")
    }
    fn cfg_with(status: StatusPreset) -> UiConfig { UiConfig { status, ..UiConfig::default() } }
    const ALL_PRESETS: [StatusPreset; 7] = [
        StatusPreset::Default, StatusPreset::Minimal, StatusPreset::Compact, StatusPreset::Full,
        StatusPreset::Nerd, StatusPreset::Ascii, StatusPreset::Custom,
    ];
    const ALL_SEPARATORS: [StatusSeparator; 7] = [
        StatusSeparator::Powerline, StatusSeparator::PowerlineThin, StatusSeparator::Slash, StatusSeparator::Pipe,
        StatusSeparator::Block, StatusSeparator::None, StatusSeparator::Ascii,
    ];

    fn busy_full() -> StatusData {
        StatusData {
            role: "subagent".into(), approval: "ask".into(), sandbox: "workspace".into(), version: Some(3), learning: "idle".into(),
            output_tokens: 1200, tokens_per_second: Some(27.54), phase: Phase::Working, elapsed_secs: 12.4, tick: 3,
            session: "abcdef123456".into(), clock: "10:54".into(), ..data()
        }
    }

    #[test]
    fn presets_map_to_their_segment_lists_and_custom_uses_the_configured_ones() {
        let names = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert_eq!(segments(&cfg_with(StatusPreset::Default)), (names(&["spinner", "model", "path", "git"]), names(&["context"])));
        assert_eq!(segments(&cfg_with(StatusPreset::Minimal)), (names(&["model"]), names(&["context"])));
        assert_eq!(segments(&cfg_with(StatusPreset::Compact)), (names(&["spinner", "model", "git"]), names(&["context", "tokens"])));
        assert_eq!(segments(&cfg_with(StatusPreset::Full)), (
            names(&["spinner", "model", "role", "path", "git", "approval", "sandbox"]),
            names(&["tokens", "rate", "learning", "context"])));
        assert_eq!(segments(&cfg_with(StatusPreset::Nerd)), segments(&cfg_with(StatusPreset::Default)));
        assert_eq!(segments(&cfg_with(StatusPreset::Ascii)), segments(&cfg_with(StatusPreset::Default)));
        let custom = UiConfig { status: StatusPreset::Custom, status_left: names(&["git", "bogus"]), status_right: names(&["time"]), ..UiConfig::default() };
        assert_eq!(segments(&custom), (names(&["git", "bogus"]), names(&["time"])));
    }

    #[test]
    fn the_line_is_exactly_the_requested_width_for_every_preset_separator_and_width() {
        let theme_ascii = Theme::dark().with_glyphs(Glyphs::Ascii);
        for status in ALL_PRESETS {
            for separator in ALL_SEPARATORS {
                let cfg = UiConfig { status, status_separator: separator, status_left: vec!["model".into(), "path".into()], status_right: vec!["context".into()], ..UiConfig::default() };
                for theme in [Theme::dark(), theme_ascii.clone(), Theme::dark().with_glyphs(Glyphs::Nerd)] {
                    for width in 0..=130u16 {
                        for d in [data(), busy_full(), StatusData::default()] {
                            let line = border_line(&cfg, &d, &theme, width, Style::default(), "╭", "╮");
                            assert_eq!(text::line_width(&line), width as usize, "{status:?} {separator:?} width {width}: {:?}", plain(&line));
                            let row = preview_row(&cfg, &d, &theme, width);
                            assert_eq!(text::line_width(&row), width as usize, "preview {status:?} width {width}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn default_line_shows_segments_in_order_with_the_context_on_the_right() {
        let line = plain(&render(&UiConfig::default(), &data(), 100));
        assert!(line.starts_with("╭─") && line.ends_with("─╮"), "{line:?}");
        let at = |needle: &str| line.find(needle).unwrap_or_else(|| panic!("{needle:?} missing in {line:?}"));
        assert!(at("mlx/qwen3-9b") < at("projects/dial") && at("projects/dial") < at("main +1 ~2 ?3") && at("main +1 ~2 ?3") < at("12%"), "{line:?}");
        assert!(line.contains("3.9k/32k"), "{line:?}");
        assert!(!line.contains("work/projects"), "path is abbreviated to its last two components: {line:?}");
        assert!(line.trim_end_matches("─╮").trim_end().ends_with("3.9k/32k"), "context sits at the right end: {line:?}");
        let full_path = UiConfig { status_path_abbreviate: false, ..UiConfig::default() };
        assert!(plain(&render(&full_path, &data(), 120)).contains("/work/projects/dial"));
        let no_counts = UiConfig { status_git_counts: false, ..UiConfig::default() };
        let line = plain(&render(&no_counts, &data(), 120));
        assert!(line.contains("main") && !line.contains("+1"), "{line:?}");
    }

    #[test]
    fn the_least_important_segments_leave_first_and_the_model_stays_longest() {
        let cfg = UiConfig::default();
        let present = |width: u16, name: &str| plain(&render(&cfg, &data(), width)).contains(name);
        let vanishes_at = |name: &str| (1..=140u16).rev().find(|w| !present(*w, name)).unwrap_or(0);
        let (git, path, context, model) = (vanishes_at("main"), vanishes_at("projects/dial"), vanishes_at("12%"), vanishes_at("mlx"));
        assert!(git > path && path > context && context > model, "git {git} path {path} context {context} model {model}");
        // Between the git and path thresholds: path and model remain, git is gone.
        let line = plain(&render(&cfg, &data(), git));
        assert!(line.contains("projects/dial") && line.contains("mlx/qwen3-9b") && !line.contains("main"), "{line:?}");
        // Once an optional segment is gone it never comes back at narrower widths... or the reverse.
        for name in ["main", "projects/dial", "12%"] {
            let mut seen_missing = false;
            for width in (20..=140u16).rev() {
                let here = present(width, name);
                assert!(!(seen_missing && here), "{name} reappears at width {width}");
                seen_missing |= !here;
            }
        }
    }

    #[test]
    fn long_texts_are_shortened_with_an_ellipsis_never_wrapped() {
        let long = StatusData { model: "a-very-long-model-name-that-will-not-fit-anywhere-near".into(), ..data() };
        let line = render(&UiConfig::default(), &long, 50);
        let text = plain(&line);
        assert_eq!(text::line_width(&line), 50);
        assert!(text.contains("mlx/a-ver…"), "the model shrinks to its floor with an ellipsis: {text:?}");
        let ascii = border_line(&UiConfig::default(), &long, &Theme::dark().with_glyphs(Glyphs::Ascii), 50, Style::default(), "+", "+");
        assert!(plain(&ascii).contains("...") && plain(&ascii).is_ascii(), "{:?}", plain(&ascii));
        let deep = StatusData { project_path: "/a/b/c/an-extraordinarily-long-directory-name".into(), ..data() };
        let cfg = UiConfig { status: StatusPreset::Custom, status_left: vec!["path".into()], status_right: vec![], ..UiConfig::default() };
        let text = plain(&render(&cfg, &deep, 30));
        assert!(text.contains("…") && text.contains("name"), "the tail of a path is kept: {text:?}");
    }

    #[test]
    fn unknown_facts_are_absent_and_an_unknown_context_is_never_zero() {
        let unknown = StatusData { prompt_tokens: None, ..data() };
        let line = plain(&render(&UiConfig::default(), &unknown, 100));
        assert!(line.contains("ctx ?") && !line.contains("0%") && !line.contains("0k"), "{line:?}");
        let no_window = StatusData { context_window: 0, ..data() };
        assert!(plain(&render(&UiConfig::default(), &no_window, 100)).contains("ctx ?"));
        let full = cfg_with(StatusPreset::Full);
        let line = plain(&render(&full, &data(), 160));
        for absent in ["tok/s", "learning", "approval", "sandbox", "v0", "↓0"] { assert!(!line.contains(absent), "{absent:?} must not appear when unknown: {line:?}"); }
        let nothing = plain(&render(&UiConfig::default(), &StatusData::default(), 80));
        assert!(nothing.contains("ctx ?") && !nothing.contains("mlx"), "{nothing:?}");
    }

    #[test]
    fn full_preset_shows_every_known_segment() {
        let line = plain(&render(&cfg_with(StatusPreset::Full), &busy_full(), 200));
        for expected in ["12s", "mlx/qwen3-9b", "subagent", "projects/dial", "main +1 ~2 ?3", "ask", "workspace", "1.2k", "27.5 tok/s", "v3 · learning idle", "12%"] {
            assert!(line.contains(expected), "{expected:?} missing: {line:?}");
        }
        let ascii = plain(&border_line(&cfg_with(StatusPreset::Full), &busy_full(), &Theme::dark().with_glyphs(Glyphs::Ascii), 200, Style::default(), "+", "+"));
        assert!(ascii.contains("approval ask") && ascii.contains("sandbox workspace"), "without icons the segments carry a word label: {ascii:?}");
        let main_role = StatusData { role: "main".into(), ..busy_full() };
        let line = plain(&render(&cfg_with(StatusPreset::Full), &main_role, 200));
        assert!(!line.contains("› main ›"), "role segment is hidden for the main role: {line:?}");
    }

    #[test]
    fn custom_lists_use_given_order_and_skip_unknown_names() {
        let cfg = UiConfig {
            status: StatusPreset::Custom, status_left: vec!["bogus".into(), "git".into(), " Model ".into(), "nope".into()],
            status_right: vec!["???".into(), "session".into(), "time".into()], ..UiConfig::default()
        };
        let line = plain(&render(&cfg, &busy_full(), 120));
        let at = |n: &str| line.find(n).unwrap_or_else(|| panic!("{n:?} missing: {line:?}"));
        assert!(at("main") < at("mlx/qwen3-9b") && at("mlx/qwen3-9b") < at("abcdef12") && at("abcdef12") < at("10:54"), "{line:?}");
        assert!(!line.contains("bogus") && !line.contains("nope") && !line.contains("???") && !line.contains("abcdef123"), "{line:?}");
        let empty = UiConfig { status: StatusPreset::Custom, status_left: vec![], status_right: vec!["bogus".into()], ..UiConfig::default() };
        assert_eq!(plain(&render(&empty, &data(), 10)), "╭────────╮");
    }

    #[test]
    fn separators_come_from_the_configured_style_and_the_active_symbol_set() {
        let cfg = |separator| UiConfig { status_separator: separator, status_left: vec![], ..UiConfig::default() };
        let line_for = |separator, theme: &Theme| {
            let cfg = UiConfig { status: StatusPreset::Custom, status_left: vec!["model".into(), "path".into()], status_right: vec![], ..cfg(separator) };
            plain(&border_line(&cfg, &data(), theme, 100, Style::default(), "╭", "╮"))
        };
        let dark = Theme::dark();
        assert!(line_for(StatusSeparator::PowerlineThin, &dark).contains("qwen3-9b › "), "unicode thin");
        assert!(line_for(StatusSeparator::Powerline, &dark).contains("qwen3-9b ▶ "));
        assert!(line_for(StatusSeparator::Slash, &dark).contains("qwen3-9b / "));
        assert!(line_for(StatusSeparator::Pipe, &dark).contains("qwen3-9b | "));
        assert!(line_for(StatusSeparator::Block, &dark).contains("qwen3-9b █ "));
        let none = line_for(StatusSeparator::None, &dark);
        assert!(none.contains("qwen3-9b  ") && !none.contains("qwen3-9b ›") && !none.contains("qwen3-9b /"), "{none:?}");
        assert!(line_for(StatusSeparator::Ascii, &dark).contains("qwen3-9b > "), "ascii separator stays ascii on a unicode theme");
        let nerd = dark.clone().with_glyphs(Glyphs::Nerd);
        assert!(line_for(StatusSeparator::PowerlineThin, &nerd).contains("qwen3-9b \u{e0b1} "));
        assert!(line_for(StatusSeparator::Powerline, &nerd).contains('\u{e0b0}'));
    }

    #[test]
    fn context_color_shifts_at_sixty_and_eighty_five_percent() {
        let theme = Theme::dark();
        let color_of = |used: u64, label: &str| {
            let d = StatusData { prompt_tokens: Some(used), context_window: 100, ..data() };
            let line = border_line(&UiConfig::default(), &d, &theme, 120, Style::default(), "╭", "╮");
            line.spans.iter().find(|s| s.content.as_ref() == label).unwrap_or_else(|| panic!("{label} not found in {:?}", plain(&line))).style.fg
        };
        assert_eq!(color_of(59, "59%"), theme.fg(Tok::Success).fg);
        assert_eq!(color_of(60, "60%"), theme.fg(Tok::Warning).fg);
        assert_eq!(color_of(84, "84%"), theme.fg(Tok::Warning).fg);
        assert_eq!(color_of(85, "85%"), theme.fg(Tok::Error).fg);
        assert_ne!(theme.fg(Tok::Success).fg, theme.fg(Tok::Error).fg);
    }

    #[test]
    fn context_bar_is_four_cells_and_human_units_are_floored() {
        let line_at = |used: u64, window: u32| {
            let d = StatusData { prompt_tokens: Some(used), context_window: window, ..data() };
            plain(&render(&UiConfig::default(), &d, 120))
        };
        assert!(line_at(50, 100).contains("50% ██·· 50/100"), "{}", line_at(50, 100));
        assert!(line_at(4, 100).contains("4% █··· "), "any usage shows at least one cell");
        assert!(line_at(100, 100).contains("100% ████ 100/100"));
        assert!(line_at(0, 100).contains("0% ···· 0/100"), "a known zero is shown as zero");
        assert!(line_at(3900, 32_768).contains("3.9k/32k"));
        assert!(line_at(4000, 131_072).contains("4k/131k"));
        assert!(line_at(1_250_000, 2_000_000).contains("1.2M/2M"));
        assert_eq!(human(999), "999");
        assert_eq!(elapsed(5.9), "5s");
        assert_eq!(elapsed(65.0), "1m05s");
        assert_eq!(elapsed(3720.0), "1h02m");
    }

    #[test]
    fn transparent_status_has_no_background_and_the_default_has_the_status_background() {
        let theme = Theme::dark();
        let bg = theme.bg(Tok::StatusBg).bg;
        assert!(bg.is_some());
        let filled = border_line(&UiConfig::default(), &data(), &theme, 100, Style::default(), "╭", "╮");
        let model = filled.spans.iter().find(|s| s.content.contains("qwen3-9b")).unwrap();
        assert_eq!(model.style.bg, bg);
        let transparent = border_line(&UiConfig { status_transparent: true, ..UiConfig::default() }, &data(), &theme, 100, Style::default(), "╭", "╮");
        assert!(transparent.spans.iter().all(|s| s.style.bg.is_none()), "{:?}", transparent.spans);
    }

    #[test]
    fn spinner_is_busy_only_and_animates_with_the_tick() {
        let cfg = UiConfig::default();
        let theme = Theme::dark();
        let at = |tick: u64, phase: Phase| plain(&border_line(&cfg, &StatusData { phase, tick, elapsed_secs: 12.0, ..data() }, &theme, 100, Style::default(), "╭", "╮"));
        let (a, b) = (at(0, Phase::Working), at(1, Phase::Working));
        assert!(a.contains(&format!("{} 12s", theme.spinner(0))) && b.contains(&format!("{} 12s", theme.spinner(1))), "{a:?} {b:?}");
        assert_ne!(a, b);
        let idle = at(0, Phase::Idle);
        assert!(!idle.contains("12s") && !idle.contains(theme.spinner(0)), "{idle:?}");
        let ascii = border_line(&cfg, &StatusData { phase: Phase::Working, tick: 1, ..data() }, &theme.clone().with_glyphs(Glyphs::Ascii), 100, Style::default(), "+", "+");
        assert!(plain(&ascii).is_ascii());
    }

    #[test]
    fn untrusted_status_text_cannot_inject_terminal_sequences() {
        let hostile = StatusData { model: "evil\x1b]0;owned\x07model".into(), branch: "br\x1b[31manch\n2".into(), ..data() };
        let line = plain(&render(&UiConfig::default(), &hostile, 120));
        assert!(!line.contains('\x1b') && !line.contains('\x07') && !line.contains('\n'), "{line:?}");
        assert!(line.contains("evilmodel") && line.contains("branch 2"), "{line:?}");
    }

    #[test]
    fn wide_and_emoji_text_is_measured_in_cells() {
        let wide = StatusData { model: "日本語モデル🦀-large".into(), project_path: "/w/日本語/プロジェクト".into(), branch: "機能/🦀".into(), ..data() };
        for width in 0..=90u16 {
            let line = render(&UiConfig::default(), &wide, width);
            assert_eq!(text::line_width(&line), width as usize, "width {width}: {:?}", plain(&line));
        }
    }

    #[test]
    fn paths_and_model_ids_are_shortened_to_what_the_user_recognizes() {
        assert_eq!(tilde("/Users/me/dev/dial", Some("/Users/me")), "~/dev/dial");
        assert_eq!(tilde("/Users/me", Some("/Users/me/")), "~");
        assert_eq!(tilde("/Users/mel/x", Some("/Users/me")), "/Users/mel/x", "a sibling directory with the same prefix is not home");
        assert_eq!(tilde("/tmp/x", None), "/tmp/x");
        assert_eq!(last_components("~/dev/projects/dial", 2), "projects/dial");
        assert_eq!(last_components("~", 2), "~");
        assert_eq!(last_components("/tmp/x", 2), "/tmp/x");
        assert_eq!(last_components("/", 2), "/");
        assert_eq!(short_model("/cache/hub/models--mlx-community--Qwen3-9B/snapshots/abc"), "mlx-community/Qwen3-9B");
        assert_eq!(short_model("/models/local/qwen.gguf"), "qwen.gguf");
        assert_eq!(short_model("qwen3:9b"), "qwen3:9b");
    }

    #[test]
    fn preview_row_has_no_corners_and_the_corner_variant_does() {
        let preview = plain(&preview_row(&UiConfig::default(), &data(), &Theme::dark(), 90));
        assert!(!preview.contains('╭') && !preview.contains('╮') && preview.contains("mlx/qwen3-9b"), "{preview:?}");
        let boxed = plain(&render(&UiConfig::default(), &data(), 90));
        assert!(boxed.starts_with('╭') && boxed.ends_with('╮'));
        let ascii = border_line(&UiConfig::default(), &data(), &Theme::dark().with_glyphs(Glyphs::Ascii), 90, Style::default(), "+", "+");
        let ascii = plain(&ascii);
        assert!(ascii.starts_with("+-") && ascii.ends_with("-+") && ascii.is_ascii(), "{ascii:?}");
    }

    #[test]
    fn border_style_applies_to_the_rule_only() {
        let border = Style::default().fg(Color::Magenta);
        let line = border_line(&UiConfig::default(), &data(), &Theme::dark(), 100, border, "╭", "╮");
        assert_eq!(line.spans.first().unwrap().style, border);
        assert_eq!(line.spans.last().unwrap().style, border);
        let model = line.spans.iter().find(|s| s.content.contains("qwen3-9b")).unwrap();
        assert_ne!(model.style.fg, Some(Color::Magenta));
    }
}

//! The status line: named segments (model, path, git, context, …) laid out into the composer's top
//! border. Everything is derived from [`StatusData`] (unknown facts are absent, never `0`), styled
//! from theme tokens and glyphs, and fitted to the exact width: when space runs out the least
//! important segments are dropped first, then the long texts are shortened with an ellipsis. The
//! line never wraps and is always exactly `width` cells.
use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::llm::model_config::{StatusPreset, StatusSeparator, UiConfig};

use super::{
    status::StatusData,
    text,
    theme::{Glyphs, Sym, Theme, Tok},
};

/// Segments below this importance are dropped before any text is shortened.
const SHRINK_BEFORE_DROPPING: u8 = 60;
const MODEL_FLOOR: usize = 10;
const PATH_FLOOR: usize = 6;
const LAST_RESORT_FLOOR: usize = 2;
const CONTEXT_BAR_CELLS: usize = 6;
const CONTEXT_HOT: f64 = 0.70;

/// Segment names for the configured preset: `(left, right)`. `custom` uses `ui.status_left/right`
/// verbatim (unknown names are skipped when rendering).
pub fn segments(cfg: &UiConfig) -> (Vec<String>, Vec<String>) {
    let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    match cfg.status {
        StatusPreset::Default | StatusPreset::Nerd | StatusPreset::Ascii => (
            names(&[
                "spinner",
                "model",
                "mode",
                "thinking",
                "inference",
                "path",
                "git",
            ]),
            names(&["workers", "context"]),
        ),
        StatusPreset::Minimal => (
            names(&["model", "thinking", "inference"]),
            names(&["context"]),
        ),
        StatusPreset::Compact => (
            names(&["spinner", "model", "thinking", "inference", "git"]),
            names(&["workers", "context", "tokens"]),
        ),
        StatusPreset::Full => (
            names(&[
                "spinner",
                "model",
                "thinking",
                "inference",
                "role",
                "path",
                "git",
                "approval",
                "sandbox",
            ]),
            names(&["workers", "tokens", "rate", "learning", "context"]),
        ),
        StatusPreset::Custom => (cfg.status_left.clone(), cfg.status_right.clone()),
    }
}

/// The composer's top border with the status segments embedded: `╭─ left ──── right ─╮`, exactly
/// `width` cells. `border` styles the rule; segments use their own tokens.
#[cfg(test)]
pub fn border_line(
    cfg: &UiConfig,
    data: &StatusData,
    theme: &Theme,
    width: u16,
    border: Style,
    left_corner: &str,
    right_corner: &str,
) -> Line<'static> {
    border_line_with(
        cfg,
        data,
        theme,
        width,
        border,
        theme.sym(Sym::BoxH),
        left_corner,
        right_corner,
    )
}

/// [`border_line`] with the rule glyph given, so a heavy box can draw a heavy top rule.
pub fn border_line_with(
    cfg: &UiConfig,
    data: &StatusData,
    theme: &Theme,
    width: u16,
    border: Style,
    bar: &str,
    left_corner: &str,
    right_corner: &str,
) -> Line<'static> {
    compose(
        cfg,
        data,
        theme,
        width as usize,
        border,
        bar,
        left_corner,
        right_corner,
    )
}

/// The same line without corners, for the settings preview.
pub fn preview_row(cfg: &UiConfig, data: &StatusData, theme: &Theme, width: u16) -> Line<'static> {
    compose(
        cfg,
        data,
        theme,
        width as usize,
        theme.fg(Tok::Border),
        theme.sym(Sym::BoxH),
        "",
        "",
    )
}

/// Live activity gets its own row in the composer, so metadata cannot crowd it out.
/// Custom layouts opt in with `spinner`; built-in layouts always expose active work.
pub fn activity_row(
    cfg: &UiConfig,
    data: &StatusData,
    theme: &Theme,
    width: u16,
) -> Option<Line<'static>> {
    if !data.busy() || width < 8 {
        return None;
    }
    if cfg.status == StatusPreset::Custom {
        let (left, right) = segments(cfg);
        if !left
            .iter()
            .chain(&right)
            .any(|n| canonical(n).is_some_and(|(n, _)| n == "spinner"))
        {
            return None;
        }
    }
    let w = usize::from(width);
    let waiting = data.stopping() || data.phase == super::status::Phase::WaitingCompute;
    let stale = data.model_progress.is_some() && data.model_silence_secs >= 15;
    let tone = if waiting || stale {
        Tok::Warning
    } else {
        Tok::Text
    };
    let marker = if waiting || stale {
        "!"
    } else {
        theme.spinner(data.tick)
    };
    let prefix = text::clip(&format!(" {marker} "), w);
    let mut spans = vec![Span::styled(prefix.clone(), theme.fg(tone))];
    let clock = if w >= 40 && data.elapsed_secs.is_finite() && data.elapsed_secs >= 0.0 {
        format!("  turn {} ", elapsed(data.elapsed_secs))
    } else {
        String::new()
    };
    // The silence warning is more useful than an animated track during a stalled request.
    let track = w >= 72 && !waiting && !stale;
    let reserved = text::width(&prefix) + text::width(&clock) + if track { 10 } else { 0 };
    let label = text::ellipsize(
        &data.activity_label(),
        w.saturating_sub(reserved),
        theme.ascii(),
    );
    spans.push(Span::styled(
        label.clone(),
        theme.fg(tone).add_modifier(ratatui::style::Modifier::BOLD),
    ));
    spans.push(Span::raw(
        " ".repeat(w.saturating_sub(reserved + text::width(&label))),
    ));
    if track {
        spans.push(Span::raw("  "));
        spans.extend(super::fx::activity_track(
            theme,
            data.tick,
            data.prompt_fraction(),
        ));
    }
    spans.push(Span::styled(clock, theme.fg(Tok::Muted)));
    Some(Line::from(spans))
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
    /// A moving highlight band over the text: `(tick, peak, glow)`. See [`super::fx::shimmer`].
    shimmer: Option<(u64, Style, Style)>,
}

impl Part {
    fn spans(&self) -> Vec<Span<'static>> {
        match self.shimmer {
            Some((tick, peak, glow)) => {
                super::fx::shimmer_with(&self.text, tick, self.style, peak, glow)
            }
            None => vec![Span::styled(self.text.clone(), self.style)],
        }
    }
}

#[derive(Clone)]
struct Seg {
    name: &'static str,
    importance: u8,
    parts: Vec<Part>,
}

impl Seg {
    fn width(&self) -> usize {
        self.parts.iter().map(|p| text::width(&p.text)).sum()
    }

    /// Shorten the shrinkable part by up to `by` cells without going under `floor` cells.
    fn shrink(&mut self, by: usize, floor: usize, ascii: bool) {
        let Some(part) = self.parts.iter_mut().find(|p| p.shrink.is_some()) else {
            return;
        };
        let current = text::width(&part.text);
        let target = floor.max(current.saturating_sub(by));
        if target >= current {
            return;
        }
        part.text = match part.shrink {
            Some(Shrink::Head) => text::ellipsize_start(&part.text, target, ascii),
            _ => text::ellipsize(&part.text, target, ascii),
        };
    }
}

fn canonical(name: &str) -> Option<(&'static str, u8)> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "model" => ("model", 100),
        "mode" => ("mode", 45),
        "context" => ("context", 90),
        "spinner" => ("spinner", 80),
        "thinking" | "reasoning" => ("thinking", 85),
        "inference" => ("inference", 75),
        "path" => ("path", 60),
        "role" => ("role", 55),
        "git" => ("git", 50),
        "approval" => ("approval", 45),
        "sandbox" => ("sandbox", 44),
        "tokens" => ("tokens", 40),
        "rate" => ("rate", 35),
        "learning" => ("learning", 30),
        "usage" => ("usage", 25),
        "session" => ("session", 20),
        "time" => ("time", 20),
        "workers" => ("workers", 70),
        _ => return None,
    })
}

struct Env<'a> {
    cfg: &'a UiConfig,
    data: &'a StatusData,
    theme: &'a Theme,
    home: Option<String>,
}

impl Env<'_> {
    fn style(&self, tok: Tok) -> Style {
        self.theme.fg(tok)
    }
    fn part(&self, value: impl Into<String>, tok: Tok) -> Part {
        Part {
            text: value.into(),
            style: self.style(tok),
            shrink: None,
            shimmer: None,
        }
    }
    fn shrinkable(&self, value: impl Into<String>, tok: Tok, how: Shrink) -> Part {
        Part {
            shrink: Some(how),
            ..self.part(value, tok)
        }
    }

    /// A section tag in the panel-header label style: uppercase, no fill.
    fn chip(&self, section: super::chrome::Section, label: &str) -> Part {
        let tag = super::chrome::tag(self.theme, section, label);
        Part {
            text: tag.content.into_owned(),
            style: tag.style,
            shrink: None,
            shimmer: None,
        }
    }

    /// The model as plain amber bold text.
    fn model_chip(&self, label: &str) -> Part {
        Part {
            text: label.to_owned(),
            style: self
                .theme
                .fg(Tok::Accent)
                .add_modifier(ratatui::style::Modifier::BOLD),
            shrink: Some(Shrink::Tail),
            shimmer: None,
        }
    }
    /// A plain word in the given tone: the path and the branch are text, not filled blocks.
    fn filled(&self, label: impl Into<String>, tok: Tok, how: Shrink) -> Part {
        Part {
            text: label.into(),
            style: self.theme.fg(tok),
            shrink: Some(how),
            shimmer: None,
        }
    }
    /// The icon plus a space, or `fallback` plus a space when the symbol set has no icon (ASCII).
    fn icon(&self, sym: Sym, tok: Tok, fallback: &str) -> Option<Part> {
        let glyph = self.theme.sym(sym);
        let label = if glyph.is_empty() { fallback } else { glyph };
        (!label.is_empty()).then(|| self.part(format!("{label} "), tok))
    }
    fn seg(&self, name: &'static str, importance: u8, parts: Vec<Option<Part>>) -> Option<Seg> {
        Some(Seg {
            name,
            importance,
            parts: parts.into_iter().flatten().collect(),
        })
    }

    fn build(&self, name: &str) -> Option<Seg> {
        let (name, importance) = canonical(name)?;
        let data = self.data;
        match name {
            "spinner" => {
                if !data.busy() {
                    return None;
                }
                let frame = self.theme.spinner(data.tick);
                let activity = data.activity_label();
                // The spinner glyph breathes through the brand colors; the activity label
                // carries a slow shimmer so a long wait still reads as live.
                let mut glyph = self.part(format!("{frame} "), Tok::Success);
                glyph.style = self
                    .style(Tok::Success)
                    .patch(super::fx::pulse(self.theme, data.tick));
                let mut label = String::new();
                if !activity.trim().is_empty() {
                    label.push_str(activity.trim());
                }
                if data.elapsed_secs.is_finite() && data.elapsed_secs >= 0.0 {
                    if !label.is_empty() {
                        label.push(' ');
                    }
                    label.push_str(&elapsed(data.elapsed_secs));
                }
                let mut part = if data.model_progress.is_some() {
                    self.shrinkable(label, Tok::Text, Shrink::Tail)
                } else {
                    self.part(label, Tok::Text)
                };
                let (peak, glow) = super::fx::shimmer_styles(self.theme);
                part.shimmer = Some((data.tick, peak, glow));
                let importance = if data.model_progress.is_some() {
                    95
                } else {
                    importance
                };
                self.seg(name, importance, vec![Some(glyph), Some(part)])
            }
            "model" => {
                let model = text::one_line(&data.model);
                let model = model.trim();
                if model.is_empty() {
                    return None;
                }
                let provider = text::one_line(&data.provider);
                let provider = provider.trim();
                let short = short_model(model);
                let label = if provider.is_empty() {
                    short
                } else {
                    format!("{provider}/{short}")
                };
                let lora = data.lora.label();
                let lora_tok = match data.lora {
                    super::status::Lora::Loaded | super::status::Lora::Used => Tok::Success,
                    super::status::Lora::Training
                    | super::status::Lora::Paused
                    | super::status::Lora::Evaluating
                    | super::status::Lora::Loading => Tok::StatusLearning,
                    _ => Tok::Muted,
                };
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconModel, Tok::StatusModel, ""),
                        Some(self.model_chip(&label)),
                        (!lora.is_empty()).then(|| {
                            self.part(format!(" {} {lora}", self.theme.sym(Sym::Dot)), lora_tok)
                        }),
                    ],
                )
            }
            "thinking" => {
                if !data.inference.is_empty()
                    && (self.cfg.status != StatusPreset::Custom
                        || self
                            .cfg
                            .status_left
                            .iter()
                            .chain(&self.cfg.status_right)
                            .any(|name| name.trim().eq_ignore_ascii_case("inference")))
                {
                    return None; // The request-specific thinking switch is in the inference segment.
                }
                let level = text::one_line(&data.thinking);
                if level.trim().is_empty() {
                    return None;
                }
                self.seg(
                    name,
                    importance,
                    vec![Some(self.chip(
                        super::chrome::Section::Learn,
                        &format!("think {}", level.trim()),
                    ))],
                )
            }
            "inference" => {
                let settings = text::one_line(&data.inference);
                if settings.trim().is_empty() {
                    return None;
                }
                self.seg(
                    name,
                    importance,
                    vec![Some(self.part(settings, Tok::StatusModel))],
                )
            }
            "role" => {
                let role = text::one_line(&data.role);
                let role = role.trim();
                if role.is_empty() || role == "main" {
                    return None;
                }
                self.seg(name, importance, vec![Some(self.part(role, Tok::Accent))])
            }
            "path" => {
                let path = text::one_line(&data.project_path);
                if path.trim().is_empty() {
                    return None;
                }
                let shown = tilde(path.trim(), self.home.as_deref());
                let shown = if self.cfg.status_path_abbreviate {
                    last_components(&shown, 2)
                } else {
                    shown
                };
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconFolder, Tok::Muted, ""),
                        Some(self.filled(shown, Tok::Muted, Shrink::Head)),
                    ],
                )
            }
            "git" => {
                let branch = text::one_line(&data.branch);
                let branch = branch.trim();
                if branch.is_empty() {
                    return None;
                }
                let counts = data.git_counts.filter(|_| self.cfg.status_git_counts);
                let dirty = data.git_counts.is_some_and(|(s, m, u)| s + m + u > 0);
                let tok = if dirty {
                    Tok::StatusGitDirty
                } else {
                    Tok::StatusGitClean
                };
                let counts = counts
                    .map(|(s, m, u)| {
                        [(s, '+'), (m, '~'), (u, '?')]
                            .iter()
                            .filter(|(n, _)| *n > 0)
                            .map(|(n, c)| format!("{c}{n}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .filter(|c| !c.is_empty());
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconGit, tok, ""),
                        Some(self.filled(branch, Tok::Muted, Shrink::Tail)),
                        counts.map(|c| self.part(format!(" {c}"), tok)),
                    ],
                )
            }
            // Chat is the default; only a mode that changes what Enter does earns a chip.
            "mode" => match data.mode {
                super::status::InputMode::Shell => self.seg(
                    name,
                    importance,
                    vec![Some(self.chip(super::chrome::Section::Warn, "shell"))],
                ),
                super::status::InputMode::Normal => None,
            },
            "context" => Some(self.context(importance)),
            "tokens" => {
                let (tokens, estimated) = data.live_output_tokens();
                if tokens == 0 {
                    return None;
                }
                let label = format!(
                    "{}{}{}",
                    self.theme.sym(Sym::ArrowDown),
                    if estimated { "~" } else { "" },
                    human(tokens)
                );
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconTokens, Tok::StatusTokens, ""),
                        Some(self.part(label, Tok::StatusTokens)),
                    ],
                )
            }
            "rate" => {
                let rate = data
                    .live_tokens_per_second()
                    .filter(|r| r.is_finite() && *r > 0.0)?;
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconRate, Tok::StatusRate, ""),
                        Some(self.part(
                            format!(
                                "{}{rate:.1} tok/s",
                                if data.model_progress.as_ref().is_some_and(|p| p.estimated) {
                                    "~"
                                } else {
                                    ""
                                }
                            ),
                            Tok::StatusRate,
                        )),
                    ],
                )
            }
            "approval" => {
                let value = text::one_line(&data.approval);
                if value.trim().is_empty() {
                    return None;
                }
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconApproval, Tok::StatusApproval, "approval"),
                        Some(self.part(value.trim(), Tok::StatusApproval)),
                    ],
                )
            }
            "sandbox" => {
                let value = text::one_line(&data.sandbox);
                if value.trim().is_empty() {
                    return None;
                }
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconSandbox, Tok::StatusSandbox, "sandbox"),
                        Some(self.part(value.trim(), Tok::StatusSandbox)),
                    ],
                )
            }
            "learning" => {
                let state = text::one_line(&data.learning);
                let mut pieces = Vec::new();
                if let Some(version) = data.version {
                    pieces.push(format!("v{version}"));
                }
                if !state.trim().is_empty() {
                    pieces.push(format!("learning {}", state.trim()));
                }
                if pieces.is_empty() {
                    return None;
                }
                let joiner = format!(" {} ", self.theme.sym(Sym::Dot));
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconLearning, Tok::StatusLearning, ""),
                        Some(self.part(pieces.join(&joiner), Tok::StatusLearning)),
                    ],
                )
            }
            "usage" => {
                let summary = text::one_line(data.usage.as_deref()?);
                if summary.trim().is_empty() {
                    return None;
                }
                self.seg(
                    name,
                    importance,
                    vec![Some(self.part(summary.trim().to_owned(), Tok::StatusRate))],
                )
            }
            "session" => {
                let id = text::one_line(&data.session);
                let id = text::clip(id.trim(), 8);
                if id.is_empty() {
                    return None;
                }
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconSession, Tok::StatusSession, ""),
                        Some(self.part(id, Tok::StatusSession)),
                    ],
                )
            }
            "workers" => {
                let tally = data.workers.as_ref().filter(|t| t.running > 0)?;
                self.seg(
                    name,
                    importance,
                    vec![Some(self.part(tally.label(), Tok::StatusTokens))],
                )
            }
            "time" => {
                let clock = text::one_line(&data.clock);
                if clock.trim().is_empty() {
                    return None;
                }
                self.seg(
                    name,
                    importance,
                    vec![
                        self.icon(Sym::IconClock, Tok::StatusTime, ""),
                        Some(self.part(clock.trim(), Tok::StatusTime)),
                    ],
                )
            }
            _ => None,
        }
    }

    /// `◔ 12% ━━──── 3.9k/32k ctx`; before the first request, the window alone (`◔ 32k ctx`);
    /// `◔ ctx ?` when even the window is unknown. Never a made-up zero. The meter and the
    /// percentage turn amber above 70%.
    fn context(&self, importance: u8) -> Seg {
        let icon = self.icon(Sym::IconContext, Tok::StatusContext, "");
        let data = self.data;
        let (Some(fraction), Some(used)) = (data.context_fraction(), data.prompt_tokens) else {
            return Seg {
                name: "context",
                importance,
                parts: icon
                    .into_iter()
                    .chain([self.part(
                        if data.context_window > 0 {
                            format!("{} ctx", human(u64::from(data.context_window)))
                        } else {
                            "ctx ?".to_owned()
                        },
                        Tok::Muted,
                    )])
                    .collect(),
            };
        };
        let hot = fraction > CONTEXT_HOT;
        let tok = if hot { Tok::Accent } else { Tok::Muted };
        let filled = ((fraction * CONTEXT_BAR_CELLS as f64).round() as usize)
            .max(usize::from(fraction > 0.0))
            .min(CONTEXT_BAR_CELLS);
        let (on, off) = if self.theme.ascii() {
            ("=", "-")
        } else {
            ("━", "─")
        };
        let bar_on = on.repeat(filled);
        let bar_off = off.repeat(CONTEXT_BAR_CELLS - filled);
        // Bar and absolute usage; the percentage only appears once it is worth a warning.
        let mut parts: Vec<Part> = icon.into_iter().collect();
        let approx = if data.context_estimated { "~" } else { "" };
        if hot {
            parts.push(self.part(format!("{}% ", (fraction * 100.0).round() as u32), tok));
        }
        parts.push(self.part(bar_on, tok));
        parts.push(self.part(bar_off, Tok::Muted));
        parts.push(self.part(
            format!(
                " {approx}{}/{}",
                human(used),
                human(u64::from(data.context_window))
            ),
            Tok::Muted,
        ));
        Seg {
            name: "context",
            importance,
            parts,
        }
    }
}

/// `12s`, `1m05s`, `1h02m`.
pub(super) fn elapsed(secs: f64) -> String {
    let total = secs as u64;
    match total {
        0..=59 => format!("{total}s"),
        60..=3599 => format!("{}m{:02}s", total / 60, total % 60),
        _ => format!("{}h{:02}m", total / 3600, total % 3600 / 60),
    }
}

/// `3.9k`, `32k`, `1.2M`: floored so the used amount never looks larger than it is.
pub(super) fn human(n: u64) -> String {
    let scaled = |unit: u64, suffix: &str| {
        let tenths = n * 10 / unit;
        if tenths.is_multiple_of(10) {
            format!("{}{suffix}", tenths / 10)
        } else {
            format!("{}.{}{suffix}", tenths / 10, tenths % 10)
        }
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
    if let Some(name) = path.components().find_map(|part| {
        part.as_os_str()
            .to_str()
            .and_then(|s| s.strip_prefix("models--"))
    }) {
        return name.replace("--", "/");
    }
    if path.is_absolute()
        && let Some(name) = path.file_name()
    {
        return name.to_string_lossy().into_owned();
    }
    model.to_owned()
}

fn tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty())
    else {
        return path.to_owned();
    };
    match path.strip_prefix(home) {
        Some("") => "~".to_owned(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

fn last_components(path: &str, keep: usize) -> String {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    if components.len() <= keep {
        return if path.len() > 1 {
            path.trim_end_matches('/').to_owned()
        } else {
            path.to_owned()
        };
    }
    components[components.len() - keep..].join("/")
}

// ---------------------------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------------------------

fn group_width(segs: &[Seg], sep: usize) -> usize {
    if segs.is_empty() {
        0
    } else {
        segs.iter().map(Seg::width).sum::<usize>() + sep * (segs.len() - 1)
    }
}

/// Cells needed by both groups, including their one-space padding and the minimum rule between them.
fn needed(left: &[Seg], right: &[Seg], sep: usize) -> usize {
    let (lw, rw) = (group_width(left, sep), group_width(right, sep));
    let padded = |w: usize| if w == 0 { 0 } else { w + 2 };
    padded(lw) + padded(rw) + usize::from(lw > 0 && rw > 0)
}

/// Remove the least important unprotected segment below `below` (ties: rightmost of the left group
/// first, then the right group). False when nothing was removable.
fn drop_least_important(
    left: &mut Vec<Seg>,
    right: &mut Vec<Seg>,
    protect_model: bool,
    below: u8,
) -> bool {
    let candidates = left
        .iter()
        .enumerate()
        .rev()
        .map(|(i, s)| (false, i, s))
        .chain(right.iter().enumerate().rev().map(|(i, s)| (true, i, s)));
    let mut best: Option<(bool, usize, u8)> = None;
    for (in_right, index, seg) in candidates {
        if seg.importance >= below || (protect_model && seg.name == "model") {
            continue;
        }
        if best.is_none_or(|(_, _, importance)| seg.importance < importance) {
            best = Some((in_right, index, seg.importance));
        }
    }
    let Some((in_right, index, _)) = best else {
        return false;
    };
    if in_right {
        right.remove(index);
    } else {
        left.remove(index);
    }
    true
}

fn shrink_texts(
    left: &mut [Seg],
    right: &mut [Seg],
    avail: usize,
    sep: usize,
    ascii: bool,
    floors: &[(&str, usize)],
) {
    for (name, floor) in floors {
        let over = needed(left, right, sep).saturating_sub(avail);
        if over == 0 {
            return;
        }
        if let Some(seg) = left
            .iter_mut()
            .chain(right.iter_mut())
            .find(|s| s.name == *name)
        {
            seg.shrink(over, *floor, ascii);
        }
    }
}

/// Make the groups fit `avail` cells: drop the minor segments, shorten path then model, drop the
/// rest by importance (the model last), and only then squeeze the model below its comfortable size.
fn fit(left: &mut Vec<Seg>, right: &mut Vec<Seg>, avail: usize, sep: usize, ascii: bool) {
    while needed(left, right, sep) > avail
        && drop_least_important(left, right, false, SHRINK_BEFORE_DROPPING)
    {}
    shrink_texts(
        left,
        right,
        avail,
        sep,
        ascii,
        &[
            ("path", PATH_FLOOR),
            ("model", MODEL_FLOOR),
            ("spinner", 24),
        ],
    );
    while needed(left, right, sep) > avail && drop_least_important(left, right, true, u8::MAX) {}
    shrink_texts(
        left,
        right,
        avail,
        sep,
        ascii,
        &[("model", LAST_RESORT_FLOOR)],
    );
    if needed(left, right, sep) > avail {
        // Without room for the icon the model name still beats nothing.
        for seg in left.iter_mut().chain(right.iter_mut()) {
            if seg.name == "model" && seg.parts.len() > 1 && seg.parts[0].shrink.is_none() {
                seg.parts.remove(0);
            }
        }
        shrink_texts(
            left,
            right,
            avail,
            sep,
            ascii,
            &[("model", LAST_RESORT_FLOOR)],
        );
    }
    if needed(left, right, sep) > avail {
        left.clear();
        right.clear();
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn separator_glyph(cfg: &UiConfig, theme: &Theme) -> String {
    match cfg.status_separator {
        // Powerline arrows only make sense between filled segments, so they render as a dot.
        StatusSeparator::Powerline | StatusSeparator::PowerlineThin => {
            theme.sym(Sym::Dot).to_owned()
        }
        StatusSeparator::Slash => theme.sym(Sym::SepSlash).to_owned(),
        StatusSeparator::Pipe => theme.sym(Sym::SepPipe).to_owned(),
        StatusSeparator::Block => theme.sym(Sym::SepBlock).to_owned(),
        StatusSeparator::None => String::new(),
        // The ASCII style is ASCII whatever the active symbol set is.
        StatusSeparator::Ascii => theme
            .clone()
            .with_glyphs(Glyphs::Ascii)
            .sym(Sym::SepPowerlineThin)
            .to_owned(),
    }
}

/// A plain rule of exactly `width` cells with the corners when they fit.
fn rule(width: usize, left: &str, right: &str, bar: &str, border: Style) -> Line<'static> {
    let corners = text::width(left) + text::width(right);
    let spans = if width >= corners {
        vec![Span::styled(
            format!("{left}{}{right}", bar.repeat(width - corners)),
            border,
        )]
    } else {
        vec![Span::styled(bar.repeat(width), border)]
    };
    Line::from(spans)
}

fn group_spans(segs: &[Seg], sep_spans: &[Span<'static>]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, seg) in segs.iter().enumerate() {
        if index > 0 {
            spans.extend(sep_spans.iter().cloned());
        }
        spans.extend(seg.parts.iter().flat_map(Part::spans));
    }
    spans
}

fn compose(
    cfg: &UiConfig,
    data: &StatusData,
    theme: &Theme,
    width: usize,
    border: Style,
    bar: &str,
    left_corner: &str,
    right_corner: &str,
) -> Line<'static> {
    let frame = text::width(left_corner) + text::width(right_corner) + 2 * text::width(bar);
    if width < frame {
        return rule(width, left_corner, right_corner, bar, border);
    }

    let env = Env {
        cfg,
        data,
        theme,
        home: std::env::var("HOME").ok(),
    };
    let (left_names, right_names) = segments(cfg);
    let build = |names: &[String]| {
        names
            .iter()
            .filter_map(|n| env.build(n))
            .collect::<Vec<_>>()
    };
    let (mut left, mut right) = (build(&left_names), build(&right_names));

    let glyph = separator_glyph(cfg, theme);
    let sep_text = if glyph.is_empty() {
        "  ".to_owned()
    } else {
        format!(" {glyph} ")
    };
    let sep_width = text::width(&sep_text);
    let sep_spans = if glyph.is_empty() {
        vec![Span::raw(sep_text)]
    } else {
        vec![
            Span::raw(" "),
            Span::styled(glyph, theme.fg(Tok::Muted)),
            Span::raw(" "),
        ]
    };

    let avail = width - frame;
    fit(&mut left, &mut right, avail, sep_width, theme.ascii());
    if left.is_empty() && right.is_empty() {
        return rule(width, left_corner, right_corner, bar, border);
    }

    let pad = |segs: &[Seg]| {
        if segs.is_empty() {
            0
        } else {
            group_width(segs, sep_width) + 2
        }
    };
    let fill = avail - pad(&left) - pad(&right);
    let padding = || Span::raw(" ");
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
mod tests;

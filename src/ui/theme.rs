//! Theme system: named color/style tokens plus symbol presets. Every UI component asks for a [`Tok`]
//! or a [`Sym`]; none hard-codes a color or glyph, so a theme file or a symbol preset restyles
//! everything at once.
//!
//! Themes are JSON (`<dial home>/themes/<name>.json`, or one of the embedded built-ins in `themes/`):
//! ```json
//! { "name": "mine", "extends": "dark", "dark": true,
//!   "vars":   { "blue": "#7aa2f7" },
//!   "colors": { "accent": "blue", "muted": 244, "text": "", "toolTitle": { "fg": "#fff", "bold": true },
//!               "userBg": "#171c26" },
//!   "symbols": { "preset": "nerd", "overrides": { "bullet": "•" }, "spinner": ["-", "\\", "|", "/"] } }
//! ```
//! A color is `"#rrggbb"` / `"#rgb"`, an ANSI-256 index (0-255), a named ANSI color (`red`, `bright-blue`…),
//! a `vars` reference, or `""` for the terminal default. An object adds `bold`, `italic`, `underline`,
//! `dim`, `reverse`, `strike`. Tokens not mentioned inherit from `extends` (default: `dark`). Unknown
//! tokens, variables and keys are errors (so typos are caught), not silently ignored.
use crate::llm::model_config::{SymbolPreset, UiConfig};
use ratatui::style::{Color, Modifier, Style};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Style tokens. Foreground tokens style text; tokens ending in `Bg` style a background.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tok {
    // core text and borders
    Text,
    Muted,
    Dim,
    Accent,
    Border,
    BorderAccent,
    BorderMuted,
    Success,
    Error,
    Warning,
    // blocks
    SelectedBg,
    UserBg,
    UserText,
    ToolPendingBg,
    ToolSuccessBg,
    ToolErrorBg,
    ToolTitle,
    ToolOutput,
    ThinkingText,
    // markdown
    MdHeading,
    MdLink,
    MdLinkUrl,
    MdCode,
    MdCodeBlock,
    MdCodeBorder,
    MdQuote,
    MdQuoteBorder,
    MdHr,
    MdBullet,
    // diffs
    DiffAdded,
    DiffRemoved,
    DiffContext,
    // composer border by activity / mode
    ThinkOff,
    ThinkMinimal,
    ThinkLow,
    ThinkMedium,
    ThinkHigh,
    ThinkXhigh,
    ShellMode,
    // status line segments
    StatusBg,
    StatusSep,
    StatusModel,
    StatusPath,
    StatusGitClean,
    StatusGitDirty,
    StatusContext,
    StatusTokens,
    StatusRate,
    StatusApproval,
    StatusSandbox,
    StatusLearning,
    StatusSession,
    StatusTime,
    SyntaxKeyword,
    SyntaxString,
    SyntaxNumber,
    SyntaxComment,
    SyntaxType,
}

impl Tok {
    pub const ALL: &'static [Tok] = &[
        Tok::Text,
        Tok::Muted,
        Tok::Dim,
        Tok::Accent,
        Tok::Border,
        Tok::BorderAccent,
        Tok::BorderMuted,
        Tok::Success,
        Tok::Error,
        Tok::Warning,
        Tok::SelectedBg,
        Tok::UserBg,
        Tok::UserText,
        Tok::ToolPendingBg,
        Tok::ToolSuccessBg,
        Tok::ToolErrorBg,
        Tok::ToolTitle,
        Tok::ToolOutput,
        Tok::ThinkingText,
        Tok::MdHeading,
        Tok::MdLink,
        Tok::MdLinkUrl,
        Tok::MdCode,
        Tok::MdCodeBlock,
        Tok::MdCodeBorder,
        Tok::MdQuote,
        Tok::MdQuoteBorder,
        Tok::MdHr,
        Tok::MdBullet,
        Tok::DiffAdded,
        Tok::DiffRemoved,
        Tok::DiffContext,
        Tok::ThinkOff,
        Tok::ThinkMinimal,
        Tok::ThinkLow,
        Tok::ThinkMedium,
        Tok::ThinkHigh,
        Tok::ThinkXhigh,
        Tok::ShellMode,
        Tok::StatusBg,
        Tok::StatusSep,
        Tok::StatusModel,
        Tok::StatusPath,
        Tok::StatusGitClean,
        Tok::StatusGitDirty,
        Tok::StatusContext,
        Tok::StatusTokens,
        Tok::StatusRate,
        Tok::StatusApproval,
        Tok::StatusSandbox,
        Tok::StatusLearning,
        Tok::StatusSession,
        Tok::StatusTime,
        Tok::SyntaxKeyword,
        Tok::SyntaxString,
        Tok::SyntaxNumber,
        Tok::SyntaxComment,
        Tok::SyntaxType,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    /// camelCase id used in theme JSON files (`"userBg"`, `"statusModel"`, …).
    pub fn id(self) -> &'static str {
        match self {
            Tok::Text => "text",
            Tok::Muted => "muted",
            Tok::Dim => "dim",
            Tok::Accent => "accent",
            Tok::Border => "border",
            Tok::BorderAccent => "borderAccent",
            Tok::BorderMuted => "borderMuted",
            Tok::Success => "success",
            Tok::Error => "error",
            Tok::Warning => "warning",
            Tok::SelectedBg => "selectedBg",
            Tok::UserBg => "userBg",
            Tok::UserText => "userText",
            Tok::ToolPendingBg => "toolPendingBg",
            Tok::ToolSuccessBg => "toolSuccessBg",
            Tok::ToolErrorBg => "toolErrorBg",
            Tok::ToolTitle => "toolTitle",
            Tok::ToolOutput => "toolOutput",
            Tok::ThinkingText => "thinkingText",
            Tok::MdHeading => "mdHeading",
            Tok::MdLink => "mdLink",
            Tok::MdLinkUrl => "mdLinkUrl",
            Tok::MdCode => "mdCode",
            Tok::MdCodeBlock => "mdCodeBlock",
            Tok::MdCodeBorder => "mdCodeBorder",
            Tok::MdQuote => "mdQuote",
            Tok::MdQuoteBorder => "mdQuoteBorder",
            Tok::MdHr => "mdHr",
            Tok::MdBullet => "mdBullet",
            Tok::DiffAdded => "diffAdded",
            Tok::DiffRemoved => "diffRemoved",
            Tok::DiffContext => "diffContext",
            Tok::ThinkOff => "thinkOff",
            Tok::ThinkMinimal => "thinkMinimal",
            Tok::ThinkLow => "thinkLow",
            Tok::ThinkMedium => "thinkMedium",
            Tok::ThinkHigh => "thinkHigh",
            Tok::ThinkXhigh => "thinkXhigh",
            Tok::ShellMode => "shellMode",
            Tok::StatusBg => "statusBg",
            Tok::StatusSep => "statusSep",
            Tok::StatusModel => "statusModel",
            Tok::StatusPath => "statusPath",
            Tok::StatusGitClean => "statusGitClean",
            Tok::StatusGitDirty => "statusGitDirty",
            Tok::StatusContext => "statusContext",
            Tok::StatusTokens => "statusTokens",
            Tok::StatusRate => "statusRate",
            Tok::StatusApproval => "statusApproval",
            Tok::StatusSandbox => "statusSandbox",
            Tok::StatusLearning => "statusLearning",
            Tok::StatusSession => "statusSession",
            Tok::StatusTime => "statusTime",
            Tok::SyntaxKeyword => "syntax.keyword",
            Tok::SyntaxString => "syntax.string",
            Tok::SyntaxNumber => "syntax.number",
            Tok::SyntaxComment => "syntax.comment",
            Tok::SyntaxType => "syntax.type",
        }
    }

    pub fn from_id(id: &str) -> Option<Tok> {
        Tok::ALL.iter().copied().find(|t| t.id() == id)
    }
    pub fn is_bg(self) -> bool {
        self.id().ends_with("Bg")
    }
}

macro_rules! symbols {
    ($($variant:ident),* $(,)?) => {
        /// Glyph tokens. Every symbol has a value in every preset (unicode, nerd, ascii).
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Sym { $($variant),* }
        impl Sym {
            pub const ALL: &'static [Sym] = &[$(Sym::$variant),*];
            /// camelCase id used in theme JSON (`"boxTl"`, `"scrollThumb"`).
            pub fn id(self) -> String {
                let name = format!("{self:?}");
                let mut chars = name.chars();
                chars.next().map(|c| c.to_lowercase().collect::<String>() + chars.as_str()).unwrap_or_default()
            }
            pub fn from_id(id: &str) -> Option<Sym> { Sym::ALL.iter().copied().find(|s| s.id() == id) }
        }
    };
}

symbols!(
    // rounded box frame and junctions
    BoxTl,
    BoxTr,
    BoxBl,
    BoxBr,
    BoxH,
    BoxV,
    TeeL,
    TeeR,
    TeeUp,
    TeeDown,
    Cross,
    // status line separators between segments and between the left/right halves
    SepPowerline,
    SepPowerlineThin,
    SepSlash,
    SepPipe,
    SepBlock,
    // status line segment icons
    IconModel,
    IconFolder,
    IconGit,
    IconContext,
    IconTokens,
    IconRate,
    IconClock,
    IconSession,
    IconApproval,
    IconSandbox,
    IconLearning,
    // transcript
    Bullet,
    Prompt,
    Check,
    Cross2,
    Warn,
    Working,
    Dot,
    Ellipsis,
    ArrowUp,
    ArrowDown,
    Delta,
    Quote,
    Hr,
    // lists and menus
    Pointer,
    ScrollThumb,
    ScrollTrack,
    RadioOn,
    RadioOff,
    IconCommand,
    IconSkill,
    IconSettings,
    IconProvider,
    IconLocal,
    IconKeyboard,
    IconTab,
    IconVision,
    IconTools,
    IconReasoning,
    IconEmbedding,
);

/// Which glyph set is active (after `auto` has been resolved).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyphs {
    Unicode,
    Nerd,
    Ascii,
}

/// How many colors the terminal can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
    Ansi16,
    None,
}

#[derive(Clone, PartialEq)]
pub struct Theme {
    name: String,
    dark: bool,
    styles: Vec<Style>,
    glyphs: Glyphs,
    /// Glyph set the theme itself prefers when the configured preset is `auto`.
    preferred_glyphs: Option<Glyphs>,
    overrides: HashMap<Sym, String>,
    spinner_frames: Option<Vec<String>>,
}

impl Theme {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn is_dark(&self) -> bool {
        self.dark
    }
    pub fn glyphs(&self) -> Glyphs {
        self.glyphs
    }
    pub fn ascii(&self) -> bool {
        self.glyphs == Glyphs::Ascii
    }

    /// Foreground (and modifiers) of a text token. Empty for `*Bg` tokens.
    pub fn fg(&self, t: Tok) -> Style {
        if t.is_bg() {
            Style::default()
        } else {
            Style {
                bg: None,
                ..self.styles[t.index()]
            }
        }
    }
    /// Background (and modifiers such as `reverse`) of a `*Bg` token. Empty for text tokens.
    pub fn bg(&self, t: Tok) -> Style {
        if t.is_bg() {
            Style {
                fg: None,
                ..self.styles[t.index()]
            }
        } else {
            Style::default()
        }
    }
    /// Text token drawn on a background token.
    pub fn on(&self, fg: Tok, bg: Tok) -> Style {
        self.fg(fg).patch(self.bg(bg))
    }

    pub fn sym(&self, s: Sym) -> &str {
        match self.overrides.get(&s) {
            Some(custom) => custom,
            None => symbol(self.glyphs, s),
        }
    }

    /// One frame of the activity spinner for an animation `tick` (~12 fps).
    pub fn spinner(&self, tick: u64) -> &str {
        const BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        const ASCII: &[&str] = &["|", "/", "-", "\\"];
        if let Some(frames) = self.spinner_frames.as_ref().filter(|f| !f.is_empty()) {
            return &frames[(tick as usize) % frames.len()];
        }
        let frames = if self.glyphs == Glyphs::Ascii {
            ASCII
        } else {
            BRAILLE
        };
        frames[(tick as usize) % frames.len()]
    }

    pub fn with_glyphs(mut self, glyphs: Glyphs) -> Self {
        self.glyphs = glyphs;
        self
    }

    /// Blue/orange instead of green/red for diffs (distinguishable with common color blindness).
    pub fn color_blind(mut self) -> Self {
        let pick = |c: Color, truecolor: &str, indexed: u8| match c {
            Color::Rgb(..) => hex_color(truecolor).unwrap_or(c),
            Color::Reset => c,
            _ => Color::Indexed(indexed),
        };
        for (tok, hex, idx) in [
            (Tok::DiffAdded, "#56b4e9", 74u8),
            (Tok::DiffRemoved, "#e69f00", 214u8),
        ] {
            let style = &mut self.styles[tok.index()];
            if let Some(c) = style.fg {
                style.fg = Some(pick(c, hex, idx));
            }
        }
        self
    }

    /// Map every RGB color down to what the terminal supports.
    pub fn downgraded(mut self, mode: ColorMode) -> Self {
        for style in &mut self.styles {
            style.fg = style
                .fg
                .map(|c| downgrade(c, mode))
                .filter(|_| mode != ColorMode::None);
            style.bg = style
                .bg
                .map(|c| downgrade(c, mode))
                .filter(|_| mode != ColorMode::None);
        }
        self
    }

    pub fn builtin(name: &str) -> Option<Theme> {
        let text = BUILTINS.iter().find(|(n, _)| *n == name)?.1;
        let resolve =
            |base: &str| Theme::builtin(base).ok_or_else(|| format!("unknown base theme {base:?}"));
        parse_theme(text, name, &resolve, BUILTIN_ROOTS.contains(&name)).ok()
    }
    pub fn dark() -> Theme {
        Self::builtin("dark").expect("embedded dark theme is valid")
    }
    pub fn light() -> Theme {
        Self::builtin("light").expect("embedded light theme is valid")
    }
    pub fn mono() -> Theme {
        Self::builtin("mono").expect("embedded mono theme is valid")
    }

    /// Fully resolved theme as JSON that [`ThemeManager`] can load back (a starting point for a custom theme).
    pub fn to_json(&self) -> String {
        let mut colors = Map::new();
        for tok in Tok::ALL {
            let style = self.styles[tok.index()];
            colors.insert(tok.id().into(), style_json(*tok, style));
        }
        let mut symbols = Map::new();
        if let Some(g) = self.preferred_glyphs {
            symbols.insert(
                "preset".into(),
                Value::String(
                    match g {
                        Glyphs::Unicode => "unicode",
                        Glyphs::Nerd => "nerd",
                        Glyphs::Ascii => "ascii",
                    }
                    .into(),
                ),
            );
        }
        if !self.overrides.is_empty() {
            let mut sorted: Vec<_> = self
                .overrides
                .iter()
                .map(|(s, v)| (s.id(), v.clone()))
                .collect();
            sorted.sort();
            symbols.insert(
                "overrides".into(),
                Value::Object(
                    sorted
                        .into_iter()
                        .map(|(k, v)| (k, Value::String(v)))
                        .collect(),
                ),
            );
        }
        if let Some(frames) = &self.spinner_frames {
            symbols.insert(
                "spinner".into(),
                Value::Array(frames.iter().cloned().map(Value::String).collect()),
            );
        }
        let mut doc = Map::new();
        doc.insert("name".into(), Value::String(self.name.clone()));
        doc.insert("dark".into(), Value::Bool(self.dark));
        doc.insert("colors".into(), Value::Object(colors));
        if !symbols.is_empty() {
            doc.insert("symbols".into(), Value::Object(symbols));
        }
        serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_default() + "\n"
    }
}

// ---------------------------------------------------------------------------------------------
// Built-ins

const BUILTINS: &[(&str, &str)] = &[
    ("dark", include_str!("../../themes/dark.json")),
    ("light", include_str!("../../themes/light.json")),
    ("mono", include_str!("../../themes/mono.json")),
    ("gruvbox", include_str!("../../themes/gruvbox.json")),
    ("nord", include_str!("../../themes/nord.json")),
    ("paper", include_str!("../../themes/paper.json")),
];
/// Themes that define every token themselves (all others extend one of these).
const BUILTIN_ROOTS: &[&str] = &["dark", "light", "mono"];

// ---------------------------------------------------------------------------------------------
// Colors

fn hex_color(text: &str) -> Option<Color> {
    let h = text.strip_prefix('#')?;
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = match h.len() {
        6 => u32::from_str_radix(h, 16).ok()?,
        3 => {
            let n = u32::from_str_radix(h, 16).ok()?;
            let (r, g, b) = ((n >> 8) & 0xf, (n >> 4) & 0xf, n & 0xf);
            (r * 17) << 16 | (g * 17) << 8 | (b * 17)
        }
        _ => return None,
    };
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn named_color(name: &str) -> Option<Color> {
    Some(match name {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "gray" | "grey" => Color::Gray,
        "bright-black" | "dark-gray" | "dark-grey" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        "bright-white" => Color::White,
        _ => return None,
    })
}

fn color_name(c: Color) -> Value {
    match c {
        Color::Reset => Value::String(String::new()),
        Color::Rgb(r, g, b) => Value::String(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(i) => Value::from(i),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "bright-black".into(),
        Color::LightRed => "bright-red".into(),
        Color::LightGreen => "bright-green".into(),
        Color::LightYellow => "bright-yellow".into(),
        Color::LightBlue => "bright-blue".into(),
        Color::LightMagenta => "bright-magenta".into(),
        Color::LightCyan => "bright-cyan".into(),
        Color::White => "bright-white".into(),
    }
}

const ANSI16_RGB: [(Color, (i32, i32, i32)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (205, 49, 49)),
    (Color::Green, (13, 188, 121)),
    (Color::Yellow, (229, 229, 16)),
    (Color::Blue, (36, 114, 200)),
    (Color::Magenta, (188, 63, 188)),
    (Color::Cyan, (17, 168, 205)),
    (Color::Gray, (229, 229, 229)),
    (Color::DarkGray, (102, 102, 102)),
    (Color::LightRed, (241, 76, 76)),
    (Color::LightGreen, (35, 209, 139)),
    (Color::LightYellow, (245, 245, 67)),
    (Color::LightBlue, (59, 142, 234)),
    (Color::LightMagenta, (214, 112, 214)),
    (Color::LightCyan, (41, 184, 219)),
    (Color::White, (255, 255, 255)),
];

fn dist(a: (i32, i32, i32), b: (i32, i32, i32)) -> i32 {
    (a.0 - b.0).pow(2) + (a.1 - b.1).pow(2) + (a.2 - b.2).pow(2)
}

fn indexed_rgb(i: u8) -> (i32, i32, i32) {
    match i {
        0..=15 => ANSI16_RGB[i as usize].1,
        16..=231 => {
            let n = i32::from(i) - 16;
            let level = |v: i32| if v == 0 { 0 } else { 55 + 40 * v };
            (level(n / 36), level((n / 6) % 6), level(n % 6))
        }
        _ => {
            let v = 8 + 10 * (i32::from(i) - 232);
            (v, v, v)
        }
    }
}

fn rgb_to_256(rgb: (i32, i32, i32)) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: i32| (0..6).min_by_key(|&i| (LEVELS[i] - v).abs()).unwrap_or(0);
    let (r, g, b) = (nearest(rgb.0), nearest(rgb.1), nearest(rgb.2));
    let cube = (LEVELS[r], LEVELS[g], LEVELS[b]);
    let cube_index = 16 + 36 * r + 6 * g + b;
    let avg = (rgb.0 + rgb.1 + rgb.2) / 3;
    let gray_step = ((avg - 8).max(0) / 10).min(23);
    let gray = 8 + 10 * gray_step;
    if dist(rgb, (gray, gray, gray)) < dist(rgb, cube) {
        (232 + gray_step) as u8
    } else {
        cube_index as u8
    }
}

fn downgrade(c: Color, mode: ColorMode) -> Color {
    match (c, mode) {
        (_, ColorMode::TrueColor) | (Color::Reset, _) => c,
        (_, ColorMode::None) => Color::Reset,
        (Color::Rgb(r, g, b), ColorMode::Ansi256) => {
            Color::Indexed(rgb_to_256((i32::from(r), i32::from(g), i32::from(b))))
        }
        (Color::Rgb(r, g, b), ColorMode::Ansi16) => {
            nearest_16((i32::from(r), i32::from(g), i32::from(b)))
        }
        (Color::Indexed(i), ColorMode::Ansi16) if i > 15 => nearest_16(indexed_rgb(i)),
        (Color::Indexed(i), ColorMode::Ansi16) => ANSI16_RGB[i as usize].0,
        _ => c,
    }
}

fn nearest_16(rgb: (i32, i32, i32)) -> Color {
    ANSI16_RGB
        .iter()
        .min_by_key(|(_, p)| dist(rgb, *p))
        .map(|(c, _)| *c)
        .unwrap_or(Color::Reset)
}

/// Color support from the environment (`NO_COLOR`, `COLORTERM`, `TERM`).
pub fn detect_color_mode(env: &dyn Fn(&str) -> Option<String>) -> ColorMode {
    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return ColorMode::None;
    }
    let colorterm = env("COLORTERM").unwrap_or_default().to_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") || env("WT_SESSION").is_some()
    {
        return ColorMode::TrueColor;
    }
    let term = env("TERM").unwrap_or_default();
    if term == "dumb" {
        return ColorMode::None;
    }
    if term.contains("256color") || env("TERM_PROGRAM").as_deref() == Some("Apple_Terminal") {
        return ColorMode::Ansi256;
    }
    ColorMode::Ansi16
}

// ---------------------------------------------------------------------------------------------
// JSON themes

const TOP_KEYS: &[&str] = &[
    "$schema",
    "name",
    "description",
    "extends",
    "dark",
    "vars",
    "colors",
    "symbols",
];
const MAX_THEME_BYTES: u64 = 256 * 1024;

fn resolve_color(
    value: &Value,
    vars: &Map<String, Value>,
    seen: &mut Vec<String>,
) -> Result<Color, String> {
    match value {
        Value::Number(n) => match n.as_u64() {
            Some(i) if i <= 255 => Ok(Color::Indexed(i as u8)),
            _ => Err(format!("color index {n} is outside 0-255")),
        },
        Value::String(s) if s.is_empty() => Ok(Color::Reset),
        Value::String(s) if s.starts_with('#') => {
            hex_color(s).ok_or_else(|| format!("invalid hex color {s:?} (use #rrggbb or #rgb)"))
        }
        Value::String(s) => {
            // A user variable shadows an ANSI color of the same name (`"blue": "#7aa2f7"`); a variable
            // that names itself (`"red": "red"`) means the terminal's color.
            if let Some(target) = vars.get(s).filter(|_| !seen.contains(s)) {
                seen.push(s.clone());
                let color = resolve_color(target, vars, seen);
                seen.pop();
                return color;
            }
            if let Some(c) = named_color(s) {
                return Ok(c);
            }
            if seen.contains(s) {
                return Err(format!("variable cycle: {} -> {s}", seen.join(" -> ")));
            }
            Err(format!("{s:?} is neither a color nor a defined variable"))
        }
        other => Err(format!("a color must be a string or number, not {other}")),
    }
}

fn token_style(tok: Tok, value: &Value, vars: &Map<String, Value>) -> Result<Style, String> {
    let Value::Object(object) = value else {
        let color = resolve_color(value, vars, &mut Vec::new())?;
        return Ok(if tok.is_bg() {
            Style::default().bg(color)
        } else {
            Style::default().fg(color)
        });
    };
    let mut style = Style::default();
    for (key, v) in object {
        match key.as_str() {
            "fg" | "bg" => {
                let color = resolve_color(v, vars, &mut Vec::new())?;
                style = if key == "fg" {
                    style.fg(color)
                } else {
                    style.bg(color)
                };
            }
            "bold" | "italic" | "underline" | "dim" | "reverse" | "strike" => {
                let on = v
                    .as_bool()
                    .ok_or_else(|| format!("{key} must be true or false"))?;
                let modifier = match key.as_str() {
                    "bold" => Modifier::BOLD,
                    "italic" => Modifier::ITALIC,
                    "underline" => Modifier::UNDERLINED,
                    "dim" => Modifier::DIM,
                    "reverse" => Modifier::REVERSED,
                    _ => Modifier::CROSSED_OUT,
                };
                if on {
                    style = style.add_modifier(modifier);
                }
            }
            other => {
                return Err(format!(
                    "unknown style key {other:?} (fg, bg, bold, italic, underline, dim, reverse, strike)"
                ));
            }
        }
    }
    Ok(style)
}

fn style_json(tok: Tok, style: Style) -> Value {
    let mods = [
        (Modifier::BOLD, "bold"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINED, "underline"),
        (Modifier::DIM, "dim"),
        (Modifier::REVERSED, "reverse"),
        (Modifier::CROSSED_OUT, "strike"),
    ];
    let has_mods = mods.iter().any(|(m, _)| style.add_modifier.contains(*m));
    let plain = if tok.is_bg() { style.bg } else { style.fg };
    if !has_mods && (tok.is_bg() && style.fg.is_none() || !tok.is_bg() && style.bg.is_none()) {
        return plain
            .map(color_name)
            .unwrap_or(Value::String(String::new()));
    }
    let mut o = Map::new();
    if let Some(c) = style.fg {
        o.insert("fg".into(), color_name(c));
    }
    if let Some(c) = style.bg {
        o.insert("bg".into(), color_name(c));
    }
    for (m, name) in mods {
        if style.add_modifier.contains(m) {
            o.insert(name.into(), Value::Bool(true));
        }
    }
    Value::Object(o)
}

fn sanitize_symbol(text: &str) -> Result<String, String> {
    if text.chars().any(char::is_control) {
        return Err("symbols must not contain control characters".into());
    }
    if crate::ui::text::width(text) > 8 {
        return Err(format!("symbol {text:?} is wider than 8 cells"));
    }
    Ok(text.to_owned())
}

/// Parse one theme document. `base` resolves `extends`; for a `root` theme every token must be defined
/// and nothing is inherited.
fn parse_theme(
    text: &str,
    stem: &str,
    base: &dyn Fn(&str) -> Result<Theme, String>,
    root: bool,
) -> Result<Theme, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let Value::Object(top) = doc else {
        return Err("a theme must be a JSON object".into());
    };
    if let Some(bad) = top.keys().find(|k| !TOP_KEYS.contains(&k.as_str())) {
        return Err(format!(
            "unknown top-level key {bad:?} (allowed: {})",
            TOP_KEYS.join(", ")
        ));
    }
    let mut theme = if root {
        Theme {
            name: stem.into(),
            dark: true,
            styles: vec![Style::default(); Tok::ALL.len()],
            glyphs: Glyphs::Unicode,
            preferred_glyphs: None,
            overrides: HashMap::new(),
            spinner_frames: None,
        }
    } else {
        let parent = top
            .get("extends")
            .map(|v| v.as_str().ok_or("extends must be a theme name"))
            .transpose()?
            .unwrap_or("dark");
        let mut parent = base(parent)?;
        parent.name = stem.into();
        parent
    };
    if let Some(dark) = top.get("dark") {
        theme.dark = dark.as_bool().ok_or("dark must be true or false")?;
    }
    let empty = Map::new();
    let vars = match top.get("vars") {
        Some(Value::Object(v)) => v,
        Some(_) => return Err("vars must be an object".into()),
        None => &empty,
    };
    if let Some(colors) = top.get("colors") {
        let Value::Object(colors) = colors else {
            return Err("colors must be an object".into());
        };
        let unknown: Vec<&str> = colors
            .keys()
            .map(String::as_str)
            .filter(|k| Tok::from_id(k).is_none())
            .collect();
        if !unknown.is_empty() {
            let valid: Vec<&str> = Tok::ALL.iter().map(|t| t.id()).collect();
            return Err(format!(
                "unknown color token(s): {}. Valid tokens: {}",
                unknown.join(", "),
                valid.join(", ")
            ));
        }
        for (key, value) in colors {
            let tok = Tok::from_id(key).ok_or_else(|| format!("unknown color token {key:?}"))?;
            theme.styles[tok.index()] =
                token_style(tok, value, vars).map_err(|e| format!("colors.{key}: {e}"))?;
        }
    }
    // Optional syntax tokens follow the palette's existing colors.
    for (token, fallback) in [
        (Tok::SyntaxKeyword, Tok::Accent),
        (Tok::SyntaxString, Tok::Success),
        (Tok::SyntaxNumber, Tok::Warning),
        (Tok::SyntaxComment, Tok::Dim),
        (Tok::SyntaxType, Tok::MdHeading),
    ] {
        if top.get("colors").and_then(|c| c.get(token.id())).is_none()
            && (root
                || top
                    .get("extends")
                    .is_none_or(|v| BUILTIN_ROOTS.contains(&v.as_str().unwrap_or(""))))
        {
            theme.styles[token.index()] = theme.styles[fallback.index()];
        }
    }
    if root {
        let missing: Vec<&str> = Tok::ALL
            .iter()
            .filter(|t| {
                !t.id().starts_with("syntax.")
                    && top.get("colors").and_then(|c| c.get(t.id())).is_none()
            })
            .map(|t| t.id())
            .collect();
        if !missing.is_empty() {
            return Err(format!("missing color token(s): {}", missing.join(", ")));
        }
    }
    if let Some(symbols) = top.get("symbols") {
        let Value::Object(symbols) = symbols else {
            return Err("symbols must be an object".into());
        };
        for key in symbols.keys() {
            if !["preset", "overrides", "spinner"].contains(&key.as_str()) {
                return Err(format!("unknown symbols key {key:?}"));
            }
        }
        if let Some(preset) = symbols.get("preset") {
            theme.preferred_glyphs = Some(match preset.as_str() {
                Some("unicode") => Glyphs::Unicode,
                Some("nerd") => Glyphs::Nerd,
                Some("ascii") => Glyphs::Ascii,
                _ => return Err("symbols.preset must be unicode, nerd or ascii".into()),
            });
        }
        if let Some(overrides) = symbols.get("overrides") {
            let Value::Object(overrides) = overrides else {
                return Err("symbols.overrides must be an object".into());
            };
            for (key, value) in overrides {
                let sym = Sym::from_id(key).ok_or_else(|| format!("unknown symbol {key:?}"))?;
                let text = value
                    .as_str()
                    .ok_or_else(|| format!("symbols.overrides.{key} must be a string"))?;
                theme.overrides.insert(
                    sym,
                    sanitize_symbol(text).map_err(|e| format!("symbols.overrides.{key}: {e}"))?,
                );
            }
        }
        if let Some(spinner) = symbols.get("spinner") {
            let frames = spinner
                .as_array()
                .ok_or("symbols.spinner must be an array of strings")?;
            if frames.is_empty() || frames.len() > 32 {
                return Err("symbols.spinner needs 1 to 32 frames".into());
            }
            theme.spinner_frames = Some(
                frames
                    .iter()
                    .map(|f| {
                        f.as_str()
                            .ok_or_else(|| "spinner frames must be strings".to_owned())
                            .and_then(sanitize_symbol)
                    })
                    .collect::<Result<_, _>>()?,
            );
        }
    }
    Ok(theme)
}

// ---------------------------------------------------------------------------------------------
// Manager

/// One theme the user can pick.
#[derive(Clone, Debug)]
pub struct ThemeEntry {
    pub name: String,
    pub builtin: bool,
    /// Why a custom theme file could not be loaded.
    pub error: Option<String>,
}

type EnvFn = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Owns the active theme: resolves `auto`, applies symbol/color-blind/color-mode settings, lists built-in
/// and custom themes (`<dial home>/themes/*.json`), supports live preview and hot reload of the active file.
pub struct ThemeManager {
    dir: PathBuf,
    env: EnvFn,
    cfg: UiConfig,
    terminal_dark: Option<bool>,
    /// The configured theme; `theme` differs only while a preview is active.
    configured: Theme,
    theme: Theme,
    watched: Option<(PathBuf, Option<SystemTime>, u64)>,
    last_error: Option<String>,
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl ThemeManager {
    /// `terminal_dark`: the terminal's reported background (`None` = unknown → dark).
    pub fn new(user_dir: &Path, cfg: &UiConfig, terminal_dark: Option<bool>) -> ThemeManager {
        Self::with_env(
            user_dir,
            cfg,
            terminal_dark,
            Box::new(|k| std::env::var(k).ok()),
        )
    }

    pub fn with_env(
        user_dir: &Path,
        cfg: &UiConfig,
        terminal_dark: Option<bool>,
        env: EnvFn,
    ) -> ThemeManager {
        let mut manager = ThemeManager {
            dir: user_dir.join("themes"),
            env,
            cfg: cfg.clone(),
            terminal_dark,
            configured: Theme::dark(),
            theme: Theme::dark(),
            watched: None,
            last_error: None,
        };
        let _ = manager.apply_config(cfg, terminal_dark);
        manager
    }

    pub fn current(&self) -> &Theme {
        &self.theme
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    /// The last load/reload problem (custom theme invalid, fell back to dark), cleared by a good load.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn custom_path(&self, name: &str) -> Option<PathBuf> {
        safe_name(name).then(|| self.dir.join(format!("{name}.json")))
    }

    fn read_custom(&self, path: &Path) -> Result<String, String> {
        let meta =
            fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if meta.len() > MAX_THEME_BYTES {
            return Err(format!(
                "{} is larger than {} KiB",
                path.display(),
                MAX_THEME_BYTES / 1024
            ));
        }
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
    }

    /// Load by name: a built-in wins over a custom file of the same name.
    fn load_named(&self, name: &str, depth: usize) -> Result<Theme, String> {
        if depth > 8 {
            return Err(format!(
                "theme {name:?}: `extends` chain is too deep or cyclic"
            ));
        }
        if let Some(theme) = Theme::builtin(name) {
            return Ok(theme);
        }
        let path = self
            .custom_path(name)
            .ok_or_else(|| format!("{name:?} is not a valid theme name"))?;
        if !path.is_file() {
            return Err(format!(
                "unknown theme {name:?} (built-in: {}; custom themes live in {})",
                BUILTINS
                    .iter()
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", "),
                self.dir.display()
            ));
        }
        let text = self.read_custom(&path)?;
        let resolve = |base: &str| self.load_named(base, depth + 1);
        parse_theme(&text, name, &resolve, false).map_err(|e| format!("theme {name:?}: {e}"))
    }

    fn finish(&self, theme: Theme, cfg: &UiConfig) -> Theme {
        let mode = detect_color_mode(&*self.env);
        let glyphs = match cfg.symbols {
            SymbolPreset::Nerd => Glyphs::Nerd,
            SymbolPreset::Unicode => Glyphs::Unicode,
            SymbolPreset::Ascii => Glyphs::Ascii,
            SymbolPreset::Auto => theme.preferred_glyphs.unwrap_or_else(|| {
                if (self.env)("DIAL_NERD_FONT")
                    .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                {
                    Glyphs::Nerd
                } else {
                    Glyphs::Unicode
                }
            }),
        };
        let mut theme = theme.with_glyphs(glyphs);
        if cfg.color_blind {
            theme = theme.color_blind();
        }
        theme.downgraded(mode)
    }

    fn wanted_name(&self, cfg: &UiConfig, terminal_dark: Option<bool>) -> String {
        // NO_COLOR is a user instruction outranking any configured theme.
        if detect_color_mode(&*self.env) == ColorMode::None {
            return "mono".into();
        }
        if cfg.theme == "auto" {
            if terminal_dark.unwrap_or(true) {
                cfg.theme_dark.clone()
            } else {
                cfg.theme_light.clone()
            }
        } else {
            cfg.theme.clone()
        }
    }

    /// Switch to the configuration's theme/symbols/color settings. On error the theme falls back to the
    /// built-in dark (so the UI stays usable) and the problem is returned and kept in `last_error`.
    pub fn apply_config(
        &mut self,
        cfg: &UiConfig,
        terminal_dark: Option<bool>,
    ) -> Result<(), String> {
        self.cfg = cfg.clone();
        self.terminal_dark = terminal_dark;
        let name = self.wanted_name(cfg, terminal_dark);
        let (loaded, error) = match self.load_named(&name, 0) {
            Ok(t) => (t, None),
            Err(e) => (Theme::dark(), Some(e)),
        };
        self.watched = if Theme::builtin(&name).is_none() && error.is_none() {
            self.custom_path(&name).map(|p| {
                let m = fs::metadata(&p).ok();
                (
                    p,
                    m.as_ref().and_then(|m| m.modified().ok()),
                    m.map_or(0, |m| m.len()),
                )
            })
        } else {
            None
        };
        self.configured = self.finish(loaded, cfg);
        self.theme = self.configured.clone();
        self.last_error = error.clone();
        error.map_or(Ok(()), Err)
    }

    /// Built-in themes first, then valid-or-broken custom files (a broken file is listed with its error).
    pub fn names(&self) -> Vec<ThemeEntry> {
        let mut entries: Vec<ThemeEntry> = BUILTINS
            .iter()
            .map(|(n, _)| ThemeEntry {
                name: (*n).into(),
                builtin: true,
                error: None,
            })
            .collect();
        let Ok(read) = fs::read_dir(&self.dir) else {
            return entries;
        };
        let mut custom: Vec<ThemeEntry> = read
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                let stem = path.file_stem()?.to_str()?.to_owned();
                if path.extension()?.to_str()? != "json"
                    || !safe_name(&stem)
                    || Theme::builtin(&stem).is_some()
                {
                    return None;
                }
                Some(ThemeEntry {
                    error: self.load_named(&stem, 0).err(),
                    name: stem,
                    builtin: false,
                })
            })
            .take(200)
            .collect();
        custom.sort_by(|a, b| a.name.cmp(&b.name));
        entries.extend(custom);
        entries
    }

    /// Preview a theme without changing the configured one; [`restore`](Self::restore) undoes it.
    pub fn preview(&mut self, name: &str) -> Result<(), String> {
        let theme = self.load_named(name, 0)?;
        self.theme = self.finish(theme, &self.cfg.clone());
        Ok(())
    }

    pub fn restore(&mut self) {
        self.theme = self.configured.clone();
    }

    /// Fully resolved JSON of a theme (built-in or custom), to copy into the themes directory and edit.
    pub fn export_json(&self, name: &str) -> Result<String, String> {
        self.load_named(name, 0).map(|t| t.to_json())
    }

    /// Poll the active custom theme file; true when it was reloaded. A broken edit keeps the last good
    /// theme and is reported through `last_error`.
    pub fn reload_if_changed(&mut self) -> bool {
        let Some((path, stamp, len)) = self.watched.clone() else {
            return false;
        };
        let Ok(meta) = fs::metadata(&path) else {
            return false;
        };
        let now = (meta.modified().ok(), meta.len());
        if now == (stamp, len) {
            return false;
        }
        self.watched = Some((path.clone(), now.0, now.1));
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        match self.load_named(&name, 0) {
            Ok(theme) => {
                self.configured = self.finish(theme, &self.cfg.clone());
                self.theme = self.configured.clone();
                self.last_error = None;
                true
            }
            Err(error) => {
                self.last_error = Some(error);
                false
            }
        }
    }
}

fn symbol(glyphs: Glyphs, s: Sym) -> &'static str {
    use Glyphs::*;
    use Sym::*;
    match (glyphs, s) {
        (Ascii, BoxTl | BoxTr | BoxBl | BoxBr | Cross | TeeL | TeeR | TeeUp | TeeDown) => "+",
        (Ascii, BoxH) => "-",
        (Ascii, BoxV) => "|",
        (_, BoxTl) => "╭",
        (_, BoxTr) => "╮",
        (_, BoxBl) => "╰",
        (_, BoxBr) => "╯",
        (_, BoxH) => "─",
        (_, BoxV) => "│",
        (_, TeeL) => "├",
        (_, TeeR) => "┤",
        (_, TeeUp) => "┴",
        (_, TeeDown) => "┬",
        (_, Cross) => "┼",
        (Nerd, SepPowerline) => "\u{e0b0}",
        (Nerd, SepPowerlineThin) => "\u{e0b1}",
        (Unicode, SepPowerline) => "▶",
        (Unicode, SepPowerlineThin) => "›",
        (Ascii, SepPowerline | SepPowerlineThin) => ">",
        (_, SepSlash) => "/",
        (_, SepPipe) => "|",
        (Ascii, SepBlock) => "#",
        (_, SepBlock) => "█",
        (Nerd, IconModel) => "\u{ec19}",
        (Nerd, IconFolder) => "\u{f07b}",
        (Nerd, IconGit) => "\u{e725}",
        (Nerd, IconContext) => "\u{f1c0}",
        (Nerd, IconTokens) => "\u{f0e7}",
        (Nerd, IconRate) => "\u{f0e4}",
        (Nerd, IconClock) => "\u{f017}",
        (Nerd, IconSession) => "\u{f4ad}",
        (Nerd, IconApproval) => "\u{f132}",
        (Nerd, IconSandbox) => "\u{f023}",
        (Nerd, IconLearning) => "\u{f0eb}",
        (Unicode, IconModel) => "◆",
        (Unicode, IconFolder) => "▸",
        (Unicode, IconGit) => "⎇",
        (Unicode, IconContext) => "◔",
        (Unicode, IconTokens) => "⇅",
        (Unicode, IconRate) => "⚡",
        (Unicode, IconClock) => "◷",
        (Unicode, IconSession) => "#",
        (Unicode, IconApproval) => "✓",
        (Unicode, IconSandbox) => "▣",
        (Unicode, IconLearning) => "✦",
        (
            Ascii,
            IconModel | IconFolder | IconGit | IconContext | IconTokens | IconRate | IconClock
            | IconSession | IconApproval | IconSandbox | IconLearning,
        ) => "",
        (Ascii, Bullet) => "*",
        (_, Bullet) => "●",
        (Ascii, Prompt) => ">",
        (_, Prompt) => "›",
        (Ascii, Check) => "+",
        (_, Check) => "✓",
        (Ascii, Cross2) => "x",
        (_, Cross2) => "✗",
        (Ascii, Warn) => "!",
        (_, Warn) => "⚠",
        (Ascii, Working) => "*",
        (_, Working) => "●",
        (Ascii, Dot) => "-",
        (_, Dot) => "·",
        (Ascii, Ellipsis) => "...",
        (_, Ellipsis) => "…",
        (Ascii, ArrowUp) => "^",
        (_, ArrowUp) => "↑",
        (Ascii, ArrowDown) => "v",
        (_, ArrowDown) => "↓",
        (Ascii, Delta) => "d",
        (_, Delta) => "Δ",
        (Ascii, Quote) => "|",
        (_, Quote) => "▎",
        (Ascii, Hr) => "-",
        (_, Hr) => "─",
        (Ascii, Pointer) => ">",
        (_, Pointer) => "›",
        (Ascii, ScrollThumb) => "#",
        (_, ScrollThumb) => "█",
        (Ascii, ScrollTrack) => "|",
        (_, ScrollTrack) => "│",
        (Ascii, RadioOn) => "(*)",
        (_, RadioOn) => "●",
        (Ascii, RadioOff) => "( )",
        (_, RadioOff) => "○",
        (Nerd, IconCommand) => "\u{f120}",
        (Nerd, IconSkill) => "\u{f0eb}",
        (Nerd, IconSettings) => "\u{f013}",
        (Nerd, IconProvider) => "\u{f111}",
        (Nerd, IconLocal) => "\u{f10c}",
        (Nerd, IconKeyboard) => "\u{f11c}",
        (Nerd, IconTab) => "\u{f0c9}",
        (Unicode, IconCommand) => "/",
        (Unicode, IconSkill) => "✦",
        (Unicode, IconSettings) => "⚙",
        (Unicode, IconProvider) => "●",
        (Unicode, IconLocal) => "○",
        (Unicode, IconKeyboard) => "⌨",
        (Unicode, IconTab) => "▤",
        (Nerd, IconVision) => "\u{f06e}",
        (Nerd, IconTools) => "\u{f0ad}",
        (Nerd, IconReasoning) => "\u{f5dc}",
        (Nerd, IconEmbedding) => "\u{f1b3}",
        // Plain text symbols avoid color emoji fallback in Unicode terminals.
        (Unicode, IconVision) => "◉",
        (Unicode, IconTools) => "⌘",
        (Unicode, IconReasoning) => "◇",
        (Unicode, IconEmbedding) => "≋",
        (Ascii, IconVision) => "V",
        (Ascii, IconTools) => "T",
        (Ascii, IconReasoning) => "R",
        (Ascii, IconEmbedding) => "E",
        (
            Ascii,
            IconCommand | IconSkill | IconSettings | IconProvider | IconLocal | IconKeyboard
            | IconTab,
        ) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvFn {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Box::new(move |k| map.get(k).cloned())
    }
    fn rgb_env() -> EnvFn {
        env(&[("COLORTERM", "truecolor")])
    }
    fn write_theme(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir.join("themes")).unwrap();
        fs::write(dir.join("themes").join(format!("{name}.json")), body).unwrap();
    }
    fn manager(dir: &Path, cfg: &UiConfig) -> ThemeManager {
        ThemeManager::with_env(dir, cfg, Some(true), rgb_env())
    }

    #[test]
    fn every_token_and_symbol_has_a_unique_id_that_round_trips() {
        let mut seen = std::collections::HashSet::new();
        for tok in Tok::ALL {
            assert!(seen.insert(tok.id()), "duplicate id {}", tok.id());
            assert_eq!(Tok::from_id(tok.id()), Some(*tok));
            assert_eq!(
                Tok::ALL[tok.index()],
                *tok,
                "ALL must be in declaration order"
            );
        }
        let ids: std::collections::HashSet<String> = Sym::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(ids.len(), Sym::ALL.len());
        assert!(Sym::ALL.iter().all(|s| Sym::from_id(&s.id()) == Some(*s)));
        assert_eq!(Sym::BoxTl.id(), "boxTl");
    }

    #[test]
    fn embedded_themes_are_valid_complete_and_distinct() {
        for (name, _) in BUILTINS {
            let theme = Theme::builtin(name).unwrap_or_else(|| panic!("{name} must load"));
            assert_eq!(theme.name(), *name);
            assert_eq!(theme.styles.len(), Tok::ALL.len());
        }
        // Root themes must define every token themselves (checked by the parser); extending ones override some.
        assert!(
            Theme::dark().fg(Tok::Accent) != Theme::builtin("gruvbox").unwrap().fg(Tok::Accent)
        );
        assert!(
            Theme::dark().is_dark()
                && !Theme::light().is_dark()
                && !Theme::builtin("paper").unwrap().is_dark()
        );
        assert!(
            Theme::mono()
                .fg(Tok::Accent)
                .add_modifier
                .contains(Modifier::BOLD),
            "mono conveys structure with modifiers"
        );
        assert!(
            Theme::mono()
                .bg(Tok::SelectedBg)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            Theme::dark().bg(Tok::UserBg).bg,
            Some(Color::Rgb(0x17, 0x1c, 0x26))
        );
    }

    #[test]
    fn background_and_text_tokens_stay_separate() {
        let theme = Theme::dark();
        assert!(theme.bg(Tok::UserBg).bg.is_some() && theme.fg(Tok::UserBg).bg.is_none());
        assert!(theme.fg(Tok::Muted).fg.is_some() && theme.bg(Tok::Muted).fg.is_none());
        let combined = theme.on(Tok::UserText, Tok::UserBg);
        assert!(combined.fg.is_some() && combined.bg.is_some());
    }

    #[test]
    fn custom_theme_resolves_vars_extends_numbers_defaults_and_modifiers() {
        let dir = tempfile::tempdir().unwrap();
        write_theme(
            dir.path(),
            "mine",
            r##"{
            "extends": "light", "dark": false,
            "vars": { "brand": "#336699", "alias": "brand", "gray": 244 },
            "colors": { "accent": "alias", "muted": "gray", "text": "", "toolTitle": { "fg": "brand", "bold": true, "italic": true },
                        "userBg": "#abc", "error": "bright-red" }
        }"##,
        );
        let cfg = UiConfig {
            theme: "mine".into(),
            ..UiConfig::default()
        };
        let m = manager(dir.path(), &cfg);
        assert!(m.last_error().is_none(), "{:?}", m.last_error());
        let t = m.current();
        assert_eq!(
            t.fg(Tok::Accent).fg,
            Some(Color::Rgb(0x33, 0x66, 0x99)),
            "variables resolve through aliases"
        );
        assert_eq!(t.fg(Tok::Muted).fg, Some(Color::Indexed(244)));
        assert_eq!(
            t.fg(Tok::Text).fg,
            Some(Color::Reset),
            "empty string means the terminal default"
        );
        assert!(
            t.fg(Tok::ToolTitle)
                .add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
        assert_eq!(
            t.bg(Tok::UserBg).bg,
            Some(Color::Rgb(0xaa, 0xbb, 0xcc)),
            "#rgb expands"
        );
        assert_eq!(t.fg(Tok::Error).fg, Some(Color::LightRed));
        assert_eq!(
            t.fg(Tok::Warning),
            Theme::light().fg(Tok::Warning),
            "unmentioned tokens inherit from extends"
        );
        assert!(!t.is_dark());
    }

    #[test]
    fn invalid_themes_are_rejected_with_actionable_messages() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "badtoken",
                r##"{"colors":{"accnt":"#fff"}}"##,
                "unknown color token(s): accnt",
            ),
            ("badkey", r#"{"colours":{}}"#, "unknown top-level key"),
            (
                "badhex",
                r##"{"colors":{"accent":"#12"}}"##,
                "invalid hex color",
            ),
            (
                "missingvar",
                r#"{"colors":{"accent":"nope"}}"#,
                "neither a color nor a defined variable",
            ),
            (
                "cycle",
                r#"{"vars":{"a":"b","b":"a"},"colors":{"accent":"a"}}"#,
                "variable cycle",
            ),
            ("range", r#"{"colors":{"accent":300}}"#, "outside 0-255"),
            (
                "style",
                r##"{"colors":{"accent":{"fg":"#fff","glow":true}}}"##,
                "unknown style key",
            ),
            ("notjson", "{", "invalid JSON"),
            ("selfext", r#"{"extends":"selfext"}"#, "cyclic"),
            ("noparent", r#"{"extends":"ghost"}"#, "unknown theme"),
            (
                "symbol",
                r#"{"symbols":{"overrides":{"nosuch":"x"}}}"#,
                "unknown symbol",
            ),
            (
                "ctrl",
                "{\"symbols\":{\"overrides\":{\"bullet\":\"\\u001b[31m\"}}}",
                "control characters",
            ),
        ];
        for (name, body, _) in cases {
            write_theme(dir.path(), name, body);
        }
        let m = manager(dir.path(), &UiConfig::default());
        let entries = m.names();
        for (name, _, expect) in cases {
            let entry = entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} not listed"));
            let error = entry
                .error
                .as_deref()
                .unwrap_or_else(|| panic!("{name} should have failed"));
            assert!(error.contains(expect), "{name}: {error}");
        }
    }

    #[test]
    fn unknown_or_hostile_names_fall_back_to_dark_and_report() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["nope", "../../etc/passwd", "a/b", ""] {
            let cfg = UiConfig {
                theme: bad.into(),
                ..UiConfig::default()
            };
            let mut m = manager(dir.path(), &UiConfig::default());
            assert!(m.apply_config(&cfg, Some(true)).is_err(), "{bad:?}");
            assert_eq!(m.current().name(), "dark");
            assert!(m.last_error().is_some());
        }
    }

    #[test]
    fn colors_downgrade_to_the_terminal_and_no_color_wins() {
        assert_eq!(
            detect_color_mode(&*env(&[("COLORTERM", "24bit")])),
            ColorMode::TrueColor
        );
        assert_eq!(
            detect_color_mode(&*env(&[("TERM", "xterm-256color")])),
            ColorMode::Ansi256
        );
        assert_eq!(
            detect_color_mode(&*env(&[("TERM", "xterm")])),
            ColorMode::Ansi16
        );
        assert_eq!(
            detect_color_mode(&*env(&[("TERM", "dumb")])),
            ColorMode::None
        );
        assert_eq!(
            detect_color_mode(&*env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])),
            ColorMode::None
        );
        assert_eq!(
            detect_color_mode(&*env(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")])),
            ColorMode::TrueColor,
            "empty NO_COLOR is ignored"
        );

        let dir = tempfile::tempdir().unwrap();
        let cfg = UiConfig::default();
        let m256 = ThemeManager::with_env(
            dir.path(),
            &cfg,
            Some(true),
            env(&[("TERM", "xterm-256color")]),
        );
        assert!(
            matches!(m256.current().fg(Tok::Accent).fg, Some(Color::Indexed(_))),
            "rgb became an index"
        );
        assert_eq!(rgb_to_256((255, 255, 255)), 231);
        assert_eq!(rgb_to_256((0, 0, 0)), 16);
        assert!(
            (232..=255).contains(&rgb_to_256((128, 128, 128))),
            "grays use the gray ramp"
        );
        let m16 = ThemeManager::with_env(dir.path(), &cfg, Some(true), env(&[("TERM", "xterm")]));
        assert!(
            !matches!(
                m16.current().fg(Tok::Accent).fg,
                Some(Color::Rgb(..) | Color::Indexed(_))
            ),
            "16-color has only named colors"
        );

        let none = ThemeManager::with_env(
            dir.path(),
            &UiConfig {
                theme: "gruvbox".into(),
                ..cfg
            },
            Some(true),
            env(&[("NO_COLOR", "1")]),
        );
        assert_eq!(
            none.current().name(),
            "mono",
            "NO_COLOR overrides the configured theme"
        );
        for tok in Tok::ALL {
            let (f, b) = (none.current().fg(*tok), none.current().bg(*tok));
            assert!(
                matches!(f.fg, None | Some(Color::Reset))
                    && matches!(b.bg, None | Some(Color::Reset)),
                "{tok:?} must carry no color"
            );
        }
    }

    #[test]
    fn symbol_presets_overrides_and_spinner() {
        let dir = tempfile::tempdir().unwrap();
        write_theme(
            dir.path(),
            "glyphs",
            r#"{"symbols":{"preset":"nerd","overrides":{"bullet":"•"},"spinner":["a","b"]}}"#,
        );
        let mut cfg = UiConfig {
            theme: "glyphs".into(),
            ..UiConfig::default()
        };
        let mut m = manager(dir.path(), &cfg);
        assert_eq!(
            m.current().glyphs(),
            Glyphs::Nerd,
            "auto follows the theme's preferred preset"
        );
        assert_eq!(m.current().sym(Sym::Bullet), "•");
        assert_eq!((m.current().spinner(0), m.current().spinner(3)), ("a", "b"));
        cfg.symbols = SymbolPreset::Ascii;
        m.apply_config(&cfg, Some(true)).unwrap();
        assert_eq!(
            m.current().glyphs(),
            Glyphs::Ascii,
            "an explicit preset beats the theme's preference"
        );
        assert!(m.current().sym(Sym::BoxTl).is_ascii() && m.current().sym(Sym::Prompt).is_ascii());
        assert_eq!(
            m.current().sym(Sym::Bullet),
            "•",
            "overrides are the user's explicit choice in every preset"
        );
        // Auto without a theme preference: unicode unless DIAL_NERD_FONT opts in.
        let plain = UiConfig::default();
        assert_eq!(
            manager(dir.path(), &plain).current().glyphs(),
            Glyphs::Unicode
        );
        let nerd = ThemeManager::with_env(
            dir.path(),
            &plain,
            Some(true),
            env(&[("DIAL_NERD_FONT", "1")]),
        );
        assert_eq!(nerd.current().glyphs(), Glyphs::Nerd);
        for sym in Sym::ALL {
            assert!(
                Theme::dark()
                    .with_glyphs(Glyphs::Ascii)
                    .sym(*sym)
                    .is_ascii(),
                "{sym:?} must have an ascii form"
            );
        }
    }

    #[test]
    fn auto_theme_picks_the_slot_for_the_terminal_background() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = UiConfig {
            theme: "auto".into(),
            theme_dark: "nord".into(),
            theme_light: "paper".into(),
            ..UiConfig::default()
        };
        assert_eq!(
            ThemeManager::with_env(dir.path(), &cfg, Some(true), rgb_env())
                .current()
                .name(),
            "nord"
        );
        assert_eq!(
            ThemeManager::with_env(dir.path(), &cfg, Some(false), rgb_env())
                .current()
                .name(),
            "paper"
        );
        assert_eq!(
            ThemeManager::with_env(dir.path(), &cfg, None, rgb_env())
                .current()
                .name(),
            "nord",
            "unknown background means dark"
        );
    }

    #[test]
    fn preview_and_restore_do_not_touch_the_configured_theme() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = manager(dir.path(), &UiConfig::default());
        m.preview("gruvbox").unwrap();
        assert_eq!(m.current().name(), "gruvbox");
        assert!(m.preview("ghost").is_err());
        assert_eq!(
            m.current().name(),
            "gruvbox",
            "a failed preview keeps the previous preview"
        );
        m.restore();
        assert_eq!(m.current().name(), "dark");
    }

    #[test]
    fn custom_themes_hot_reload_and_a_broken_edit_keeps_the_last_good_theme() {
        let dir = tempfile::tempdir().unwrap();
        write_theme(dir.path(), "live", r##"{"colors":{"accent":"#111111"}}"##);
        let cfg = UiConfig {
            theme: "live".into(),
            ..UiConfig::default()
        };
        let mut m = manager(dir.path(), &cfg);
        assert_eq!(
            m.current().fg(Tok::Accent).fg,
            Some(Color::Rgb(0x11, 0x11, 0x11))
        );
        assert!(!m.reload_if_changed(), "unchanged file is not reloaded");
        let path = dir.path().join("themes/live.json");
        fs::write(&path, r##"{"colors":{"accent":"#222222"}, "dark": true}"##).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert!(m.reload_if_changed());
        assert_eq!(
            m.current().fg(Tok::Accent).fg,
            Some(Color::Rgb(0x22, 0x22, 0x22))
        );
        fs::write(&path, "{ not json").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();
        assert!(!m.reload_if_changed());
        assert_eq!(
            m.current().fg(Tok::Accent).fg,
            Some(Color::Rgb(0x22, 0x22, 0x22)),
            "last good theme stays active"
        );
        assert!(m.last_error().is_some_and(|e| e.contains("invalid JSON")));
        assert!(
            !m.reload_if_changed(),
            "the same broken file is not retried every poll"
        );
    }

    #[test]
    fn built_in_names_win_and_listing_includes_custom_themes() {
        let dir = tempfile::tempdir().unwrap();
        write_theme(dir.path(), "dark", r##"{"colors":{"accent":"#ff0000"}}"##);
        write_theme(dir.path(), "zebra", "{}");
        write_theme(dir.path(), "alpha", "{}");
        let m = manager(dir.path(), &UiConfig::default());
        assert_eq!(
            m.current().fg(Tok::Accent),
            Theme::dark().fg(Tok::Accent),
            "a custom file cannot shadow a built-in"
        );
        let names: Vec<(String, bool)> =
            m.names().into_iter().map(|e| (e.name, e.builtin)).collect();
        let custom: Vec<&str> = names
            .iter()
            .filter(|(_, b)| !*b)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(custom, ["alpha", "zebra"]);
        assert!(
            names.iter().take(BUILTINS.len()).all(|(_, b)| *b),
            "built-ins first"
        );
    }

    #[test]
    fn exported_themes_round_trip_and_color_blind_mode_only_changes_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path(), &UiConfig::default());
        for name in ["dark", "mono", "gruvbox"] {
            let json = m.export_json(name).unwrap();
            let reloaded = parse_theme(
                &json,
                name,
                &|b| Theme::builtin(b).ok_or_else(|| "no".into()),
                true,
            )
            .unwrap();
            let original = Theme::builtin(name).unwrap();
            assert_eq!(
                reloaded.styles, original.styles,
                "{name} must survive export -> load"
            );
        }
        let normal = Theme::dark();
        let blind = Theme::dark().color_blind();
        for tok in Tok::ALL {
            let changed = normal.fg(*tok) != blind.fg(*tok);
            assert_eq!(
                changed,
                matches!(tok, Tok::DiffAdded | Tok::DiffRemoved),
                "{tok:?}"
            );
        }
        assert_ne!(
            blind.fg(Tok::DiffAdded).fg,
            Some(Color::Rgb(0x9e, 0xce, 0x6a))
        );
    }

    #[test]
    fn built_in_themes_use_their_declared_rgb_colors_not_the_terminals_named_ones() {
        // The shipped themes name variables `blue`, `red`, `gray`…; they must resolve to the RGB values
        // declared in the file, never to the terminal palette entries of the same name.
        let dark = Theme::dark();
        assert_eq!(dark.fg(Tok::Accent).fg, Some(Color::Rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(
            dark.fg(Tok::DiffAdded).fg,
            Some(Color::Rgb(0x9e, 0xce, 0x6a))
        );
        assert_eq!(dark.fg(Tok::Error).fg, Some(Color::Rgb(0xf7, 0x76, 0x8e)));
        assert_eq!(dark.fg(Tok::Muted).fg, Some(Color::Rgb(0x6b, 0x72, 0x80)));
        for name in ["dark", "light", "gruvbox", "nord", "paper"] {
            let theme = Theme::builtin(name).unwrap();
            let named = Tok::ALL
                .iter()
                .filter(|t| {
                    matches!(
                        theme.fg(**t).fg,
                        Some(
                            Color::Red
                                | Color::Blue
                                | Color::Green
                                | Color::Yellow
                                | Color::Cyan
                                | Color::Gray
                        )
                    )
                })
                .count();
            assert_eq!(
                named, 0,
                "{name} must not fall back to terminal-named colors"
            );
        }
    }

    #[test]
    fn a_variable_shadows_a_named_color_but_a_self_reference_means_the_terminal_color() {
        let vars: Map<String, Value> =
            serde_json::from_str(r##"{"red":"#010203","blue":"blue","loop":"loop"}"##).unwrap();
        let resolve = |s: &str| resolve_color(&Value::String(s.into()), &vars, &mut Vec::new());
        assert_eq!(resolve("red"), Ok(Color::Rgb(1, 2, 3)));
        assert_eq!(
            resolve("blue"),
            Ok(Color::Blue),
            "`blue` -> `blue` is the ANSI color"
        );
        assert_eq!(
            resolve("green"),
            Ok(Color::Green),
            "unshadowed names still work"
        );
        assert!(resolve("loop").unwrap_err().contains("cycle"));
    }
}

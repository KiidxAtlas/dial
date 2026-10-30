//! `/image settings`: per-model generation settings for the image model `image_gen` currently
//! resolves (`[image.models.<key>]`).
//!
//! Only the rows the model's image API accepts are shown; values set for another API are kept but
//! not sent. Choices cycle with Left/Right, numbers step the same way (Enter types an exact value),
//! and the full LoRA list is a TOML array. Every change is validated and saved immediately; an unset
//! value leaves the endpoint's own default.
use super::{
    DrawCtx, OverlayEvent,
    model_settings::{as_number, number_text},
    settings::{
        Axis, arrows, editor_spans, filled, fit, hint, hint_line, other_scope, pair_label, panel,
        pressed, scope_name,
    },
};
use crate::config::image::{
    BACKGROUNDS, ImageLora, ImageModelSettings, LORA_SCALE_LIMIT, MAX_STEPS, MODERATIONS,
    QUALITIES, SIZES,
};
use crate::config::settings_store::Scope;
use crate::ui::{
    chrome::{self, Section},
    editor::Editor,
    keys::{Action, Keymap},
    text,
    theme::{Sym, Tok},
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::Modifier,
    text::{Line, Span},
};

#[derive(Clone)]
enum Kind {
    Choice(Vec<String>),
    Number {
        min: f64,
        max: f64,
        step: f64,
        start: f64,
        int: bool,
    },
    Toml,
    /// Comma or space separated numbers, saved as a TOML array.
    Numbers,
}

#[derive(Clone)]
struct Field {
    /// Row key: an `ImageModelSettings` field, or `lora`/`lora_scale` for the first LoRA.
    path: &'static str,
    /// The `ImageModelSettings` field the image API must accept for the row to appear.
    setting: &'static str,
    label: &'static str,
    section: &'static str,
    kind: Kind,
    help: &'static str,
}

/// The first LoRA's file and scale; `loras` edits the whole list.
const LORA: &str = "lora";
const LORA_SCALE: &str = "lora_scale";
const COND_WEIGHTS: &str = "cond_weights";

fn fields(installed_loras: &[String]) -> Vec<Field> {
    let choice = |options: &[&str]| Kind::Choice(options.iter().map(|s| (*s).to_owned()).collect());
    vec![
        Field {
            path: "size",
            setting: "size",
            label: "Default size",
            section: "Output",
            kind: choice(SIZES),
            help: "Size used when a call does not choose one. Providers that take an aspect ratio get the matching ratio. Default is 1024x1024.",
        },
        Field {
            path: "quality",
            setting: "quality",
            label: "Quality",
            section: "Output",
            kind: choice(QUALITIES),
            help: "Rendering quality. gpt-image models take auto, low, medium or high; DALL·E 3 takes standard or hd. Higher quality costs more and takes longer.",
        },
        Field {
            path: "background",
            setting: "background",
            label: "Background",
            section: "Output",
            kind: choice(BACKGROUNDS),
            help: "Transparent backgrounds need PNG or WebP output.",
        },
        Field {
            path: "moderation",
            setting: "moderation",
            label: "Moderation",
            section: "Output",
            kind: choice(MODERATIONS),
            help: "Content filtering strictness; low is less restrictive. The provider's policy still applies.",
        },
        Field {
            path: "steps",
            setting: "steps",
            label: "Steps",
            section: "Diffusion",
            kind: Kind::Number {
                min: 1.0,
                max: f64::from(MAX_STEPS),
                step: 1.0,
                start: 8.0,
                int: true,
            },
            help: "Denoising steps per image. Turbo and distilled models need few (4–8); more steps are slower and rarely better for them.",
        },
        Field {
            path: "seed",
            setting: "seed",
            label: "Seed",
            section: "Diffusion",
            kind: Kind::Number {
                min: 0.0,
                max: 4_294_967_295.0,
                step: 1.0,
                start: 0.0,
                int: true,
            },
            help: "A fixed seed repeats the same image for the same prompt and settings. Default picks a new random seed per image. Enter types an exact seed.",
        },
        Field {
            path: "strength",
            setting: "strength",
            label: "Edit strength",
            section: "Diffusion",
            kind: Kind::Number {
                min: 0.0,
                max: 1.0,
                step: 0.05,
                start: 1.0,
                int: false,
            },
            help: "How far `/image edit` may move from the source image: low keeps it close, 1.0 repaints freely. Used only for edits.",
        },
        Field {
            path: COND_WEIGHTS,
            setting: "cond_weights",
            label: "Layer weights",
            section: "Diffusion",
            kind: Kind::Numbers,
            help: "One number per tapped text-encoder layer, comma or space separated (1 = neutral). The count depends on the model (for example FLUX 3, Krea 12); MLX-Serve rejects a wrong count. Blank resets to off.",
        },
        Field {
            path: LORA,
            setting: "loras",
            label: "LoRA",
            section: "LoRA",
            kind: Kind::Choice(installed_loras.to_vec()),
            help: "Adapter applied to every image, chosen from ~/.mlx-serve/models/loras. Enter types a file name there or an absolute path. Reset removes it.",
        },
        Field {
            path: LORA_SCALE,
            setting: "loras",
            label: "LoRA scale",
            section: "LoRA",
            kind: Kind::Number {
                min: -LORA_SCALE_LIMIT,
                max: LORA_SCALE_LIMIT,
                step: 0.05,
                start: 1.0,
                int: false,
            },
            help: "Strength of the LoRA above; 1.0 is full strength. Reset returns to 1.0.",
        },
        Field {
            path: "loras",
            setting: "loras",
            label: "All LoRAs",
            section: "LoRA",
            kind: Kind::Toml,
            help: "Every adapter in order, for stacking several: [{ path = \"a.safetensors\", scale = 1.0 }, …].",
        },
        Field {
            path: "input",
            setting: "input",
            label: "Model input",
            section: "Provider",
            kind: Kind::Toml,
            help: "Model-specific fal or Replicate input as a TOML inline table. Prompt and single-image output fields are controlled by Dial. Use the model's documented input schema.",
        },
        Field {
            path: "comfyui",
            setting: "comfyui",
            label: "Workflow",
            section: "Provider",
            kind: Kind::Toml,
            help: "An inline table with workflow (the full API-format node graph), prompt_node and output_node. The prompt replaces inputs.text; dimensions and sampling remain in the workflow. See docs/image-generation.md.",
        },
    ]
}

pub struct ImageSettingsOverlay {
    /// Key under `image.models` this entry saves to.
    pub key: String,
    api: String,
    settings: ImageModelSettings,
    fields: Vec<Field>,
    cursor: usize,
    scroll: usize,
    scope: Scope,
    editor: Option<Editor>,
    pending: Option<(ImageModelSettings, String)>,
    message: Option<(String, bool)>,
}

impl ImageSettingsOverlay {
    /// `supported` lists the `ImageModelSettings` fields the model's image API accepts.
    pub fn new(
        key: String,
        api: &str,
        settings: ImageModelSettings,
        supported: &[&str],
        installed_loras: &[String],
    ) -> Self {
        let fields = fields(installed_loras)
            .into_iter()
            .filter(|field| supported.contains(&field.setting))
            .collect();
        Self {
            key,
            api: api.to_owned(),
            settings,
            fields,
            cursor: 0,
            scroll: 0,
            scope: Scope::Project,
            editor: None,
            pending: None,
            message: None,
        }
    }

    fn value(&self, path: &str) -> Option<toml::Value> {
        let first = self.settings.loras.first();
        match path {
            LORA => first.map(|lora| toml::Value::String(lora.path.clone())),
            LORA_SCALE => first.map(|lora| toml::Value::Float(lora.scale)),
            path => toml::Value::try_from(&self.settings)
                .expect("serializable image settings")
                .get(path)
                .cloned(),
        }
    }

    /// The settings with the row at `path` set to `value` (unset for `None`), if they validate.
    fn with(&self, path: &str, value: Option<toml::Value>) -> anyhow::Result<ImageModelSettings> {
        let mut next = self.settings.clone();
        match (path, value) {
            (LORA, None) => {
                if !next.loras.is_empty() {
                    next.loras.remove(0);
                }
            }
            (LORA, Some(value)) => {
                let path = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Enter a file name"))?
                    .to_owned();
                match next.loras.first_mut() {
                    Some(lora) => lora.path = path,
                    None => next.loras.push(ImageLora { path, scale: 1.0 }),
                }
            }
            (LORA_SCALE, value) => {
                let lora = next
                    .loras
                    .first_mut()
                    .ok_or_else(|| anyhow::anyhow!("Choose a LoRA first"))?;
                lora.scale = match value {
                    None => 1.0,
                    Some(value) => {
                        as_number(&value).ok_or_else(|| anyhow::anyhow!("Enter a number"))?
                    }
                };
            }
            (path, value) => {
                let mut table = toml::Table::try_from(&next).expect("serializable image settings");
                match value {
                    Some(value) => table.insert(path.to_owned(), value),
                    None => table.remove(path),
                };
                next = toml::Value::Table(table)
                    .try_into()
                    .map_err(|e: toml::de::Error| anyhow::anyhow!("{}", e.message()))?;
            }
        }
        next.validate()?;
        Ok(next)
    }

    fn candidate(&self, path: &str, text: &str) -> anyhow::Result<ImageModelSettings> {
        if text.trim().is_empty() {
            return self.with(path, None);
        }
        if path == COND_WEIGHTS {
            let numbers = text
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|token| !token.is_empty())
                .map(|token| {
                    token
                        .parse::<f64>()
                        .ok()
                        .filter(|n| n.is_finite())
                        .map(toml::Value::Float)
                        .ok_or_else(|| anyhow::anyhow!("{token:?} is not a number"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            return self.with(path, Some(toml::Value::Array(numbers)));
        }
        let parsed: toml::Table = format!("value = {text}").parse().map_err(|_| {
            anyhow::anyhow!(
                "Not a valid value: numbers are bare, text needs \"quotes\", lists use [ ]"
            )
        })?;
        anyhow::ensure!(parsed.len() == 1, "Enter one value, not extra settings");
        self.with(path, parsed.get("value").cloned())
    }

    pub fn banner(&mut self, text: String, error: bool) {
        self.message = Some((text, error));
    }

    /// The controller's answer to the last `Set`.
    pub fn settled(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                if let Some((settings, what)) = self.pending.take() {
                    self.settings = settings;
                    self.message = Some((format!("{what} · applies to the next image"), false));
                }
                self.editor = None;
            }
            Err(error) => {
                self.pending = None;
                self.message = Some((
                    error.lines().next().unwrap_or("Could not save").to_owned(),
                    true,
                ));
            }
        }
    }

    fn apply(&mut self, text: &str) -> OverlayEvent {
        let field = self.fields[self.cursor].clone();
        match self.candidate(field.path, text) {
            Ok(settings) => {
                let what = if text.trim().is_empty() {
                    format!("{} reset to default", field.label)
                } else {
                    format!("{} saved", field.label)
                };
                let value = toml::Value::try_from(&settings).expect("serializable image settings");
                self.pending = Some((settings, what));
                self.message = None;
                OverlayEvent::Set {
                    path: format!("image.models.{}", self.key),
                    value,
                    scope: self.scope,
                }
            }
            Err(error) => {
                let message = error.to_string();
                self.message = Some((
                    message.lines().next().unwrap_or("Invalid value").to_owned(),
                    true,
                ));
                OverlayEvent::None
            }
        }
    }

    /// Left/Right: cycle a choice (including the default slot) or nudge a number.
    fn step(&mut self, direction: i64) -> OverlayEvent {
        let field = self.fields[self.cursor].clone();
        let current = self.value(field.path);
        let literal = match &field.kind {
            Kind::Choice(options) if options.is_empty() => {
                self.message = Some(("No options found; Enter types a value".into(), false));
                return OverlayEvent::None;
            }
            Kind::Choice(options) => {
                let at = current
                    .as_ref()
                    .and_then(toml::Value::as_str)
                    .and_then(|text| options.iter().position(|option| option == text))
                    .map_or(0, |i| i as i64 + 1);
                let next = (at + direction).rem_euclid(options.len() as i64 + 1);
                (next > 0)
                    .then(|| toml::Value::String(options[next as usize - 1].clone()).to_string())
            }
            Kind::Number {
                min,
                max,
                step,
                start,
                int,
            } => Some(match current.as_ref().and_then(as_number) {
                None => number_text(*start, *int),
                Some(v) => number_text((v + step * direction as f64).clamp(*min, *max), *int),
            }),
            Kind::Toml | Kind::Numbers => return OverlayEvent::None,
        };
        self.apply(&literal.unwrap_or_default())
    }

    fn begin_edit(&mut self) -> OverlayEvent {
        let field = &self.fields[self.cursor];
        let shown = match (&field.kind, self.value(field.path)) {
            (_, None) => String::new(),
            (Kind::Choice(_), Some(toml::Value::String(text))) => text,
            (_, Some(value)) => value.to_string(),
        };
        let mut editor = Editor::default();
        editor.set(&shown);
        self.editor = Some(editor);
        self.message = None;
        OverlayEvent::None
    }

    pub fn event(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Event::Paste(pasted) = event {
            if let Some(editor) = &mut self.editor {
                editor.insert(pasted);
            }
            return OverlayEvent::None;
        }
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if !pressed(key) {
            return OverlayEvent::None;
        }
        if keys.matches(Action::SelectCancel, key) {
            self.message = None;
            return if self.editor.take().is_some() {
                OverlayEvent::None
            } else {
                OverlayEvent::Close
            };
        }
        if let Some(editor) = &mut self.editor {
            if keys.matches(Action::SelectConfirm, key) {
                let text = editor.text.trim().to_owned();
                // Choice rows take plain typed text; save it as a TOML string unless blank (reset).
                if matches!(self.fields[self.cursor].kind, Kind::Choice(_)) && !text.is_empty() {
                    return self.apply(&toml::Value::String(text).to_string());
                }
                return self.apply(&text);
            }
            if editor.key(*key) {
                self.message = None;
            }
            return OverlayEvent::None;
        }
        if self.fields.is_empty() {
            return OverlayEvent::None;
        }
        let last = self.fields.len() - 1;
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(last);
        } else if keys.matches(Action::SelectConfirm, key) {
            return self.begin_edit();
        } else {
            match key.code {
                KeyCode::Left => return self.step(-1),
                KeyCode::Right => return self.step(1),
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = last,
                KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('r') => {
                    if self.value(self.fields[self.cursor].path).is_some() {
                        return self.apply("");
                    }
                    self.message = Some(("Already using the default".into(), false));
                    return OverlayEvent::None;
                }
                KeyCode::Char('s') => {
                    self.scope = other_scope(self.scope);
                    self.message = Some((
                        format!("Changes now save to {} settings", scope_name(self.scope)),
                        false,
                    ));
                    return OverlayEvent::None;
                }
                _ => {}
            }
        }
        self.message = None;
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let name = self
            .settings
            .model
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.settings.model)
            .to_owned();
        panel(
            frame,
            ctx.theme,
            Section::Dial,
            "Image settings",
            &name,
            |width, height| self.compose(width, height, ctx),
        );
    }

    fn show(value: &toml::Value) -> String {
        match value {
            toml::Value::String(s) => s.clone(),
            toml::Value::Float(f) => number_text(*f, false),
            toml::Value::Array(items) => items
                .iter()
                .map(|item| match (item.get("path"), item.get("scale")) {
                    (Some(path), Some(scale)) => {
                        format!("{} × {}", Self::show(path), Self::show(scale))
                    }
                    _ => item.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", "),
            other => other.to_string(),
        }
    }

    fn compose(&mut self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let theme = ctx.theme;
        let dim = theme.fg(Tok::Muted);
        let dot = theme.sym(Sym::Dot);
        let mut out: Vec<Line<'static>> = vec![Line::from(fit(
            vec![
                Span::styled(format!(" {} {dot} ", self.api), dim),
                Span::styled(
                    self.settings.model.clone(),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                ),
            ],
            w,
        ))];
        let customs = self
            .fields
            .iter()
            // The first LoRA's rows are views of `loras`; count that setting once.
            .filter(|f| !matches!(f.path, LORA | LORA_SCALE) && self.value(f.path).is_some())
            .count();
        let summary = match customs {
            0 => "using the endpoint's defaults".to_owned(),
            1 => "1 custom value".to_owned(),
            n => format!("{n} custom values"),
        };
        out.push(Line::from(fit(
            vec![Span::styled(
                format!(
                    " {summary} {dot} saves to {} settings {dot} image.models.{}",
                    scope_name(self.scope),
                    self.key
                ),
                dim,
            )],
            w,
        )));

        if self.fields.is_empty() {
            out.push(Line::from(Span::styled(
                format!(" {} accepts no adjustable settings.", self.api),
                dim,
            )));
            out.push(hint_line(
                theme,
                &[hint(ctx.keys, Action::SelectCancel, "close")],
                w,
            ));
            return out;
        }

        let detail_h = if h >= 18 { 4 } else { 0 };
        let list_h = h.saturating_sub(2 + 1 + 1 + detail_h).max(1);
        let mut rows: Vec<(Option<&'static str>, usize)> = Vec::new();
        let mut last = "";
        for (i, f) in self.fields.iter().enumerate() {
            if f.section != last {
                rows.push((Some(f.section), i));
                last = f.section;
            }
            rows.push((None, i));
        }
        let selected_row = rows
            .iter()
            .position(|(s, i)| s.is_none() && *i == self.cursor)
            .unwrap_or(0);
        // Keep the selected row and, when it opens a section, that section's header in view.
        let top = if selected_row > 0 && rows[selected_row - 1].0.is_some() {
            selected_row - 1
        } else {
            selected_row
        };
        if top < self.scroll {
            self.scroll = top;
        }
        if selected_row >= self.scroll + list_h {
            self.scroll = selected_row + 1 - list_h;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(list_h));

        let label_w = self
            .fields
            .iter()
            .map(|f| text::width(f.label))
            .max()
            .unwrap_or(10)
            .min(w.saturating_sub(12).max(8));
        for (section, index) in rows.iter().skip(self.scroll).take(list_h) {
            if let Some(name) = section {
                let rule = theme.sym(Sym::BoxH);
                let head = format!(" {rule}{rule} ");
                let title = chrome::label(theme, name);
                let pad = w.saturating_sub(text::width(&head) + text::width(&title.content) + 1);
                out.push(Line::from(fit(
                    vec![
                        Span::styled(head, theme.fg(Tok::BorderMuted)),
                        title,
                        Span::styled(format!(" {}", rule.repeat(pad)), theme.fg(Tok::BorderMuted)),
                    ],
                    w,
                )));
                continue;
            }
            let field = &self.fields[*index];
            let selected = *index == self.cursor;
            let value = self.value(field.path);
            let marker = if value.is_some() { dot } else { " " };
            let mut spans = vec![
                Span::styled(
                    format!(
                        " {} ",
                        if selected {
                            theme.sym(Sym::Pointer)
                        } else {
                            " "
                        }
                    ),
                    theme.fg(Tok::Accent),
                ),
                Span::styled(marker.to_owned(), theme.fg(Tok::Accent)),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{:<label_w$}",
                        text::ellipsize(field.label, label_w, theme.ascii())
                    ),
                    theme.fg(Tok::Text),
                ),
                Span::raw("  "),
            ];
            let value_w = w.saturating_sub(5 + label_w + 2 + 1);
            if selected && let Some(editor) = &self.editor {
                spans.extend(editor_spans(theme, editor, value_w, false));
            } else {
                let shown = value
                    .as_ref()
                    .map_or_else(|| "default".to_owned(), Self::show);
                let adjustable = selected && !matches!(field.kind, Kind::Toml | Kind::Numbers);
                let (left, right) = if theme.ascii() {
                    ("<", ">")
                } else {
                    ("‹", "›")
                };
                let text = if adjustable {
                    format!("{left} {shown} {right}")
                } else {
                    shown
                };
                let style = if adjustable {
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD)
                } else if value.is_some() {
                    theme.fg(Tok::Text).add_modifier(Modifier::BOLD)
                } else {
                    dim
                };
                spans.push(Span::styled(
                    text::ellipsize(&text, value_w, theme.ascii()),
                    style,
                ));
            }
            out.push(if selected {
                filled(spans, w, theme.bg(Tok::SelectedBg))
            } else {
                Line::from(fit(spans, w))
            });
        }
        while out.len() < 2 + list_h {
            out.push(Line::default());
        }

        if detail_h > 0 {
            let field = &self.fields[self.cursor];
            let mut lines = vec![Line::from(fit(
                vec![chrome::label(theme, &format!(" {}", field.label))],
                w,
            ))];
            let help = text::wrap_words(field.help, w.saturating_sub(2).max(8));
            for row in help.iter().take(detail_h - 2) {
                lines.push(Line::from(fit(
                    vec![Span::styled(
                        text::ellipsize(&format!(" {row}"), w, theme.ascii()),
                        dim,
                    )],
                    w,
                )));
            }
            let accepts = match &field.kind {
                Kind::Number { min, max, int, .. } => format!(
                    "Accepts {} to {}",
                    number_text(*min, *int),
                    number_text(*max, *int)
                ),
                Kind::Choice(options) if options.is_empty() => {
                    "Enter types a value; blank resets to default".to_owned()
                }
                Kind::Choice(options) => format!("Choose: {} · default", options.join(" · ")),
                Kind::Toml => "Enter edits a TOML list; blank resets to default".to_owned(),
                Kind::Numbers => {
                    "Enter numbers, comma or space separated; count depends on the model; blank resets to off".to_owned()
                }
            };
            lines.truncate(detail_h - 1);
            while lines.len() < detail_h - 1 {
                lines.push(Line::default());
            }
            lines.push(Line::from(fit(
                vec![Span::styled(
                    text::ellipsize(&format!(" {accepts}"), w, theme.ascii()),
                    dim,
                )],
                w,
            )));
            out.extend(lines);
        }

        out.push(match &self.message {
            Some((message, true)) => Line::from(fit(
                vec![Span::styled(
                    format!(" {} {message}", theme.sym(Sym::Cross2)),
                    theme.fg(Tok::Error),
                )],
                w,
            )),
            Some((message, false)) => Line::from(fit(
                vec![Span::styled(
                    format!(" {} {message}", theme.sym(Sym::Check)),
                    theme.fg(Tok::Success),
                )],
                w,
            )),
            None if self.editor.is_some() => Line::from(fit(
                vec![Span::styled(" Type a value, then Enter to save", dim)],
                w,
            )),
            None => Line::default(),
        });

        let keys = ctx.keys;
        let segments: Vec<String> = if self.editor.is_some() {
            vec![
                hint(keys, Action::SelectConfirm, "save"),
                hint(keys, Action::SelectCancel, "cancel"),
            ]
        } else {
            vec![
                format!(
                    "{} move",
                    pair_label(keys, Action::SelectUp, Action::SelectDown)
                ),
                format!("{} change", arrows(theme, Axis::Horizontal)),
                hint(keys, Action::SelectConfirm, "edit"),
                "r reset".to_owned(),
                format!("s save to {}", scope_name(other_scope(self.scope))),
                hint(keys, Action::SelectCancel, "close"),
            ]
        };
        out.push(hint_line(theme, &segments, w));
        out
    }
}

#[cfg(test)]
mod tests;

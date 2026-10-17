//! `/model settings`: a profile editor for the current main model.
//!
//! Values are grouped into sections. Toggles and choices cycle with Left/Right, numbers step the same
//! way (Enter types an exact value), and anything else is a TOML literal. Every change is validated
//! against the provider capabilities and saved immediately; an unset value follows the provider default.
use super::{
    DrawCtx, OverlayEvent,
    settings::{
        Axis, arrows, editor_spans, filled, fit, hint, hint_line, other_scope, pair_label, panel,
        pressed, scope_name,
    },
};
use crate::{
    llm::{
        local_evidence::{self, Current, Suggestion},
        local_server,
        profiles::{ModelProfile, ReasoningApi},
    },
    storage::settings_store::Scope,
    ui::{
        chrome::{self, Section},
        editor::Editor,
        keys::{Action, Keymap},
        text,
        theme::{Sym, Tok},
    },
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    style::Modifier,
    text::{Line, Span},
};
use std::path::Path;

#[derive(Clone, Copy)]
enum Kind {
    Number {
        min: f64,
        max: f64,
        step: f64,
        start: f64,
        int: bool,
    },
    Choice {
        options: &'static [&'static str],
        optional: bool,
    },
    Toggle,
    Toml,
}

struct Field {
    path: &'static str,
    label: &'static str,
    section: &'static str,
    kind: Kind,
    help: &'static str,
}

const EFFORTS: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

const fn float(min: f64, max: f64, step: f64, start: f64) -> Kind {
    Kind::Number {
        min,
        max,
        step,
        start,
        int: false,
    }
}
const fn int(min: f64, max: f64, step: f64, start: f64) -> Kind {
    Kind::Number {
        min,
        max,
        step,
        start,
        int: true,
    }
}

static FIELDS: &[Field] = &[
    Field {
        path: "context_window",
        label: "Context size",
        section: "Context",
        kind: int(2048.0, 2_097_152.0, 1024.0, 32768.0),
        help: "This model's history/compaction cap, limited by model/server capacity. Reset follows shared defaults. Press d to change the local/cloud defaults for future model selections; this does not resize an external server.",
    },
    Field {
        path: "temperature",
        label: "Temperature",
        section: "SAMPLING",
        kind: float(0.0, 2.0, 0.05, 0.2),
        help: "Randomness of the output. Lower is more deterministic and usually better for coding.",
    },
    Field {
        path: "top_p",
        label: "Top-p",
        section: "SAMPLING",
        kind: float(0.0, 1.0, 0.05, 0.95),
        help: "Nucleus sampling: only the most likely tokens whose probabilities add up to this value are considered.",
    },
    Field {
        path: "max_tokens",
        label: "Max output tokens",
        section: "Output",
        kind: int(1.0, 1_048_576.0, 1024.0, 8192.0),
        help: "Upper bound on tokens generated per model response, including reasoning where the provider counts it.",
    },
    Field {
        path: "reasoning_effort",
        label: "Reasoning effort",
        section: "Reasoning",
        kind: Kind::Choice {
            options: EFFORTS,
            optional: true,
        },
        help: "How much the model thinks before answering. Higher costs more time and tokens.",
    },
    Field {
        path: "thinking",
        label: "Thinking",
        section: "Reasoning",
        kind: Kind::Toggle,
        help: "Force extended thinking on or off. Default lets the provider and reasoning effort decide.",
    },
    Field {
        path: "thinking_budget",
        label: "Thinking budget",
        section: "Reasoning",
        kind: int(1024.0, 1_048_576.0, 1024.0, 4096.0),
        help: "Token budget for Anthropic-style extended thinking. Must stay below the output limit.",
    },
    Field {
        path: "prompt_cache",
        label: "Prompt caching",
        section: "Caching",
        kind: Kind::Toggle,
        help: "Reuse the stable prompt prefix between requests to cut cost and latency. Default is automatic.",
    },
    Field {
        path: "cache_retention",
        label: "Cache lifetime",
        section: "Caching",
        kind: Kind::Choice {
            options: &["auto", "short", "long"],
            optional: false,
        },
        help: "How long cached prompt prefixes live. Auto uses 1 hour for subscription sign-in and 5 minutes otherwise.",
    },
    Field {
        path: "edit_mode",
        label: "Edit style",
        section: "Agent behavior",
        kind: Kind::Choice {
            options: &["hashline", "replace", "apply_patch", "patch", "sloppy"],
            optional: true,
        },
        help: "Pin this exact model's edit grammar: replace, hashline, apply_patch, structured patch or sloppy matching.",
    },
    Field {
        path: "tools",
        label: "Tool allow-list",
        section: "Agent behavior",
        kind: Kind::Toml,
        help: "Limit which tools this model sees, e.g. [\"read\", \"grep\", \"edit\"]. Blank offers the normal set.",
    },
    Field {
        path: "local_sampling.top_k",
        label: "Top-k",
        section: "Local sampling",
        kind: int(0.0, 1000.0, 5.0, 40.0),
        help: "Only sample from the k most likely tokens. 0 disables the limit.",
    },
    Field {
        path: "local_sampling.min_p",
        label: "Min-p",
        section: "Local sampling",
        kind: float(0.0, 1.0, 0.01, 0.05),
        help: "Drop tokens less likely than this fraction of the best token. A good alternative to top-p.",
    },
    Field {
        path: "local_sampling.seed",
        label: "Seed",
        section: "Local sampling",
        kind: int(0.0, 4_294_967_295.0, 1.0, 0.0),
        help: "Fixed random seed for repeatable output. Needs the seed capability to be declared.",
    },
    Field {
        path: "local_sampling.xtc_probability",
        label: "XTC probability",
        section: "Local sampling",
        kind: float(0.0, 1.0, 0.05, 0.1),
        help: "Chance of excluding top choices to increase variety. 0 turns XTC off.",
    },
    Field {
        path: "local_sampling.xtc_threshold",
        label: "XTC threshold",
        section: "Local sampling",
        kind: float(0.0, 1.0, 0.05, 0.1),
        help: "Probability above which a token may be excluded by XTC.",
    },
    Field {
        path: "local_sampling.repetition_penalty",
        label: "Repetition penalty",
        section: "Repetition",
        kind: float(0.0, 3.0, 0.05, 1.1),
        help: "Discourage repeating recent tokens. 1.0 is neutral; slightly above 1 reduces loops.",
    },
    Field {
        path: "local_sampling.repetition_context_size",
        label: "Repetition window",
        section: "Repetition",
        kind: int(0.0, 8192.0, 16.0, 64.0),
        help: "How many recent tokens the repetition penalty looks back over.",
    },
    Field {
        path: "local_sampling.presence_penalty",
        label: "Presence penalty",
        section: "Repetition",
        kind: float(-2.0, 2.0, 0.1, 0.0),
        help: "Penalize any token that already appeared, encouraging new topics.",
    },
    Field {
        path: "local_sampling.presence_context_size",
        label: "Presence window",
        section: "Repetition",
        kind: int(0.0, 8192.0, 16.0, 64.0),
        help: "How many recent tokens the presence penalty considers.",
    },
    Field {
        path: "local_sampling.frequency_penalty",
        label: "Frequency penalty",
        section: "Repetition",
        kind: float(-2.0, 2.0, 0.1, 0.0),
        help: "Penalize tokens in proportion to how often they appeared.",
    },
    Field {
        path: "local_sampling.frequency_context_size",
        label: "Frequency window",
        section: "Repetition",
        kind: int(0.0, 8192.0, 16.0, 64.0),
        help: "How many recent tokens the frequency penalty considers.",
    },
    Field {
        path: "local_sampling.chat_template_kwargs",
        label: "Template arguments",
        section: "Chat template",
        kind: Kind::Toml,
        help: "Extra arguments for the model's chat template, e.g. { enable_thinking = false }.",
    },
    Field {
        path: "capabilities",
        label: "Declared capabilities",
        section: "Advanced",
        kind: Kind::Toml,
        help: "What this exact model supports, one line per capability. Settings are validated against it. Enter opens the exact values as TOML.",
    },
];

fn shown(field: &Field, profile: &ModelProfile) -> bool {
    let mlx = profile.provider == "mlx";
    match field.section {
        "Local sampling" | "Repetition" => {
            let field = field.path.strip_prefix("local_sampling.");
            mlx || match profile.provider.as_str() {
                "lm-studio" => {
                    field.is_some_and(crate::llm::local_sampling::LocalSampling::lmstudio_field)
                }
                "ollama" => {
                    field.is_some_and(crate::llm::local_sampling::LocalSampling::ollama_field)
                }
                _ => false,
            }
        }
        "Chat template" => mlx,
        "Caching" => !mlx && profile.provider != "lm-studio",
        "Reasoning" => !mlx || profile.capabilities.reasoning != ReasoningApi::None,
        _ => true,
    }
}

pub struct ModelSettingsOverlay {
    pub name: String,
    profile: ModelProfile,
    fields: Vec<&'static Field>,
    cursor: usize,
    scroll: usize,
    scope: Scope,
    editor: Option<Editor>,
    pending: Option<(ModelProfile, String)>,
    message: Option<(String, bool)>,
    /// Evidence-backed setting suggestions for a local model (see `with_suggestions`); applied by Enter.
    suggestions: Vec<Suggestion>,
    /// The suggestion whose save is in flight; removed from the list once the save succeeds.
    staged_suggestion: Option<usize>,
}

impl ModelSettingsOverlay {
    pub fn new(name: String, profile: ModelProfile) -> Self {
        let fields = FIELDS.iter().filter(|f| shown(f, &profile)).collect();
        Self {
            name,
            profile,
            fields,
            cursor: 0,
            scroll: 0,
            scope: Scope::Project,
            editor: None,
            pending: None,
            message: None,
            suggestions: Vec::new(),
            staged_suggestion: None,
        }
    }

    fn table(&self) -> toml::Table {
        toml::Value::try_from(&self.profile)
            .expect("serializable model profile")
            .as_table()
            .unwrap()
            .clone()
    }

    fn value(&self, path: &str) -> Option<toml::Value> {
        crate::ui::settings_schema::get(&self.table(), path).cloned()
    }

    fn is_override(&self, field: &Field) -> bool {
        match field.path {
            "capabilities" => false,
            "cache_retention" => self
                .value(field.path)
                .is_some_and(|v| v.as_str() != Some("auto")),
            path => self.value(path).is_some(),
        }
    }

    fn unsupported(&self, field: &Field) -> Option<&'static str> {
        let c = &self.profile.capabilities;
        match field.path {
            "temperature" if !c.temperature => Some(
                "Not declared as supported for this model. Declare it under Advanced → capabilities.",
            ),
            "top_p" if !c.top_p => Some(
                "Not declared as supported for this model. Declare it under Advanced → capabilities.",
            ),
            "reasoning_effort" if c.reasoning_efforts.is_empty() => Some(
                "No reasoning efforts are declared for this model. Declare them under Advanced → capabilities.",
            ),
            "thinking" | "thinking_budget" if c.reasoning == ReasoningApi::None => Some(
                "This model has no declared reasoning API. Declare one under Advanced → capabilities.",
            ),
            "prompt_cache" | "cache_retention" if !c.prompt_cache => {
                Some("Prompt caching is not declared supported for this model.")
            }
            "local_sampling.seed" if !c.seed => Some(
                "Ignored unless the seed capability is declared under Advanced → capabilities.",
            ),
            _ => None,
        }
    }

    fn choices(&self, field: &Field) -> Vec<String> {
        match field.kind {
            Kind::Choice { options, .. } => {
                if field.path == "reasoning_effort"
                    && !self.profile.capabilities.reasoning_efforts.is_empty()
                {
                    self.profile
                        .capabilities
                        .reasoning_efforts
                        .iter()
                        .map(|e| e.as_str().to_owned())
                        .collect()
                } else {
                    options.iter().map(|s| (*s).to_owned()).collect()
                }
            }
            _ => Vec::new(),
        }
    }

    fn show_value(value: &toml::Value) -> String {
        match value {
            toml::Value::String(s) => s.clone(),
            toml::Value::Float(f) => number_text(*f, false),
            other => other.to_string().lines().collect::<Vec<_>>().join("; "),
        }
    }

    /// One line per declared capability in plain words, for the value column. The exact TOML stays
    /// one Enter away: opening the row loads it into the editor unchanged.
    fn capability_summary(value: &toml::Value) -> String {
        const LABELS: [(&str, &str); 11] = [
            ("vision", "Image input"),
            ("prompt_cache", "Prompt cache"),
            ("tools", "Tools"),
            ("reasoning", "Reasoning"),
            ("reasoning_efforts", "Reasoning efforts"),
            ("temperature", "Temperature"),
            ("top_p", "Top-p"),
            ("seed", "Seed"),
            ("strict_tools", "Strict tools"),
            ("parallel_tool_calls", "Parallel tool calls"),
            ("output_limit", "Output limit"),
        ];
        let Some(table) = value.as_table() else {
            return Self::show_value(value);
        };
        LABELS
            .iter()
            .map(|(key, label)| {
                let shown = match table.get(*key) {
                    None => "not declared".to_owned(),
                    Some(toml::Value::Boolean(true)) => "yes".to_owned(),
                    Some(toml::Value::Boolean(false)) => "no".to_owned(),
                    Some(toml::Value::Array(items)) if items.is_empty() => "none".to_owned(),
                    Some(toml::Value::Array(items)) => items
                        .iter()
                        .map(Self::show_value)
                        .collect::<Vec<_>>()
                        .join(", "),
                    Some(other) => Self::show_value(other),
                };
                format!("{label}: {shown}")
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// Local models only: fills the suggestion list from the evidence recorded for this model. `base`
    /// carries the configuration (default edit grammar, side-model policy, global draft model); the
    /// profile being edited overrides it. Nothing is applied here.
    pub fn with_suggestions(mut self, user_dir: &Path, base: Current) -> Self {
        if !is_local_provider(&self.profile.provider) {
            return self;
        }
        // Unreadable evidence means no suggestions; it never blocks the settings page.
        let Ok(records) = local_evidence::read(user_dir) else {
            return self;
        };
        let key = local_evidence::model_key(&self.profile.provider, &self.profile.model);
        if let Some(evidence) = records.get(&key) {
            let current = self.current(base);
            self.suggestions = local_evidence::suggestions(evidence, &current);
        }
        self
    }

    fn current(&self, mut base: Current) -> Current {
        let profile = &self.profile;
        base.provider = profile.provider.clone();
        base.model = profile.model.clone();
        base.profile_context_window = profile.context_window;
        let sampling = profile.local_sampling.as_ref();
        base.presence_penalty = sampling.and_then(|s| s.presence_penalty);
        base.repetition_penalty = sampling.and_then(|s| s.repetition_penalty);
        base.keep_alive = profile.keep_alive.clone();
        base.draft_model = profile
            .local_runtime
            .as_ref()
            .and_then(|r| r.draft_model.clone())
            .or(base.draft_model);
        if let Some(mode) = profile.edit_mode {
            base.edit_mode = serde_json::to_value(mode)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or(base.edit_mode);
            base.edit_mode_pinned = true;
        }
        if profile.provider == "mlx" {
            base.draft_candidate = local_server::draft_candidates(&profile.model)
                .into_iter()
                .next();
        }
        base
    }

    /// Fields first, then the suggestion rows under their own heading.
    fn len(&self) -> usize {
        self.fields.len() + self.suggestions.len()
    }

    fn section_at(&self, cursor: usize) -> &'static str {
        self.fields.get(cursor).map_or("Suggestions", |f| f.section)
    }

    fn candidate(&self, field: &str, text: &str) -> anyhow::Result<ModelProfile> {
        if text.trim().is_empty() {
            return self.profile_with(field, None);
        }
        let parsed: toml::Table = format!("value = {text}").parse().map_err(|_| {
            anyhow::anyhow!(
                "Not a valid value: numbers and true/false are bare, text needs \"quotes\""
            )
        })?;
        anyhow::ensure!(parsed.len() == 1, "Enter one value, not extra settings");
        let value = parsed
            .get("value")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Enter a value"))?;
        self.profile_with(field, Some(value))
    }

    /// The profile with `field` set to `value` (unset for `None`), if it still validates.
    fn profile_with(
        &self,
        field: &str,
        value: Option<toml::Value>,
    ) -> anyhow::Result<ModelProfile> {
        let mut table = self.table();
        match value {
            Some(value) => {
                crate::ui::settings_schema::set(&mut table, field, value);
            }
            None => {
                crate::ui::settings_schema::unset(&mut table, field);
            }
        }
        let profile: ModelProfile = toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| anyhow::anyhow!("{}", e.message()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Enter on a suggestion row: a profile change saves the profile, a global key saves that config
    /// key. Either way the change is only staged here; it is removed from the list once saved.
    fn apply_suggestion(&mut self, index: usize) -> OverlayEvent {
        let Some(suggestion) = self.suggestions.get(index).cloned() else {
            return OverlayEvent::None;
        };
        let value = if suggestion.value.is_null() {
            None
        } else {
            match toml::Value::try_from(&suggestion.value) {
                Ok(value) => Some(value),
                Err(error) => {
                    self.message =
                        Some((format!("Suggestion is not a setting value: {error}"), true));
                    return OverlayEvent::None;
                }
            }
        };
        let what = format!("{} applied", suggestion.title);
        if suggestion.global {
            self.staged_suggestion = Some(index);
            self.pending = Some((self.profile.clone(), what));
            self.message = None;
            return match value {
                Some(value) => OverlayEvent::Set {
                    path: suggestion.path,
                    value,
                    scope: self.scope,
                },
                None => OverlayEvent::Unset {
                    path: suggestion.path,
                    scope: self.scope,
                },
            };
        }
        match self.profile_with(&suggestion.path, value) {
            Ok(profile) => {
                let value = toml::Value::try_from(&profile).expect("serializable model profile");
                self.staged_suggestion = Some(index);
                self.pending = Some((profile, what));
                self.message = None;
                OverlayEvent::Set {
                    path: format!("model_profiles.{}", self.name),
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

    fn suggestion_detail(&self, index: usize, w: usize, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let theme = ctx.theme;
        let Some(suggestion) = self.suggestions.get(index) else {
            return Vec::new();
        };
        let mut lines = vec![Line::from(fit(
            vec![chrome::label(
                theme,
                &format!(" Suggestion: {}", suggestion.title),
            )],
            w,
        ))];
        let rows = text::wrap_words(&suggestion.evidence, w.saturating_sub(2).max(8));
        for row in rows.iter().take(2) {
            lines.push(Line::from(fit(
                vec![Span::styled(
                    text::ellipsize(&format!(" {row}"), w, theme.ascii()),
                    theme.fg(Tok::Muted),
                )],
                w,
            )));
        }
        lines.push(Line::from(fit(
            vec![Span::styled(
                " Enter applies it. Nothing changes until you press Enter.",
                theme.fg(Tok::Muted),
            )],
            w,
        )));
        lines
    }

    fn suggestion_row(
        &self,
        index: usize,
        w: usize,
        ctx: &DrawCtx,
        label_w: usize,
    ) -> Line<'static> {
        let theme = ctx.theme;
        let suggestion = &self.suggestions[index];
        let selected = self.fields.len() + index == self.cursor;
        let prefix_w = 5 + label_w + 2;
        let spans = vec![
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
            Span::raw("  "),
            Span::styled(
                format!(
                    "{:<label_w$}",
                    text::ellipsize(&suggestion.title, label_w, theme.ascii())
                ),
                theme.fg(Tok::Text),
            ),
            Span::raw("  "),
            Span::styled(
                text::ellipsize(
                    &suggestion.evidence,
                    w.saturating_sub(prefix_w + 1),
                    theme.ascii(),
                ),
                if selected {
                    theme.fg(Tok::Accent)
                } else {
                    theme.fg(Tok::Muted)
                },
            ),
        ];
        if selected {
            filled(spans, w, theme.bg(Tok::SelectedBg))
        } else {
            Line::from(fit(spans, w))
        }
    }

    pub fn banner(&mut self, text: String, error: bool) {
        self.message = Some((text, error));
    }

    pub fn settled(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                if let Some((profile, what)) = self.pending.take() {
                    self.profile = profile;
                    self.message = Some((format!("{what} · applies to the next request"), false));
                }
                if let Some(index) = self.staged_suggestion.take()
                    && index < self.suggestions.len()
                {
                    self.suggestions.remove(index);
                    self.cursor = self.cursor.min(self.len() - 1);
                }
                self.editor = None;
            }
            Err(error) => {
                self.pending = None;
                self.staged_suggestion = None;
                self.message = Some((
                    error.lines().next().unwrap_or("Could not save").to_owned(),
                    true,
                ));
            }
        }
    }

    fn apply(&mut self, text: &str) -> OverlayEvent {
        let field = self.fields[self.cursor];
        match self.candidate(field.path, text) {
            Ok(profile) => {
                let what = if text.trim().is_empty() {
                    format!("{} reset to default", field.label)
                } else {
                    format!("{} saved", field.label)
                };
                let value = toml::Value::try_from(&profile).expect("serializable model profile");
                self.pending = Some((profile, what));
                self.message = None;
                OverlayEvent::Set {
                    path: format!("model_profiles.{}", self.name),
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

    /// Left/Right: cycle a choice or toggle, or nudge a number. Unset numbers start at a sensible value.
    fn step(&mut self, direction: i64) -> OverlayEvent {
        if self.cursor >= self.fields.len() {
            return OverlayEvent::None;
        }
        let field = self.fields[self.cursor];
        let current = self.value(field.path);
        let literal = match field.kind {
            Kind::Toggle => {
                let options = [None, Some(true), Some(false)];
                let at = options
                    .iter()
                    .position(|o| *o == current.as_ref().and_then(toml::Value::as_bool))
                    .unwrap_or(0) as i64;
                options[(at + direction).rem_euclid(3) as usize].map(|b| b.to_string())
            }
            Kind::Choice { optional, .. } => {
                let list = self.choices(field);
                let lead = i64::from(optional);
                let at = current
                    .as_ref()
                    .and_then(toml::Value::as_str)
                    .and_then(|s| list.iter().position(|o| o == s))
                    .map_or(0, |i| i as i64 + lead);
                let next = (at + direction).rem_euclid(list.len() as i64 + lead);
                (next >= lead).then(|| format!("\"{}\"", list[(next - lead) as usize]))
            }
            Kind::Number {
                min,
                max,
                step,
                start,
                int,
            } => Some(match current.as_ref().and_then(as_number) {
                None => number_text(start, int),
                Some(v) => number_text((v + step * direction as f64).clamp(min, max), int),
            }),
            Kind::Toml => return OverlayEvent::None,
        };
        self.apply(&literal.unwrap_or_default())
    }

    fn begin_edit(&mut self) -> OverlayEvent {
        let field = self.fields[self.cursor];
        match field.kind {
            Kind::Number { .. } | Kind::Toml => {
                let mut editor = Editor::default();
                editor.set(
                    &self
                        .value(field.path)
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                );
                self.editor = Some(editor);
                self.message = None;
                OverlayEvent::None
            }
            _ => self.step(1),
        }
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
                let text = editor.text.clone();
                return self.apply(&text);
            }
            if editor.key(*key) {
                self.message = None;
            }
            return OverlayEvent::None;
        }
        let last = self.len() - 1;
        let section = self.section_at(self.cursor);
        if keys.matches(Action::SelectUp, key) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if keys.matches(Action::SelectDown, key) {
            self.cursor = (self.cursor + 1).min(last);
        } else if keys.matches(Action::SelectPageUp, key) {
            self.cursor = self.cursor.saturating_sub(6);
        } else if keys.matches(Action::SelectPageDown, key) {
            self.cursor = (self.cursor + 6).min(last);
        } else if key.code == KeyCode::Tab {
            // Jump to the first row of the next section.
            if let Some(next) = self.fields.iter().position(|f| {
                f.section != section && {
                    self.fields.iter().position(|g| g.section == f.section) > Some(self.cursor)
                }
            }) {
                self.cursor = next;
            }
        } else if key.code == KeyCode::BackTab {
            // Jump to the start of this section, or of the previous one when already there.
            let first = self
                .fields
                .iter()
                .position(|f| f.section == section)
                .unwrap_or(0);
            let target = if self.cursor > first {
                first
            } else {
                first.saturating_sub(1)
            };
            let name = self.fields[target].section;
            self.cursor = self
                .fields
                .iter()
                .position(|f| f.section == name)
                .unwrap_or(0);
        } else if keys.matches(Action::SelectConfirm, key) {
            if self.cursor >= self.fields.len() {
                return self.apply_suggestion(self.cursor - self.fields.len());
            }
            return self.begin_edit();
        } else {
            match key.code {
                KeyCode::Left => return self.step(-1),
                KeyCode::Right => return self.step(1),
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = last,
                KeyCode::Delete | KeyCode::Backspace | KeyCode::Char('r') => {
                    if self.cursor < self.fields.len() && self.is_override(self.fields[self.cursor])
                    {
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
                KeyCode::Char('d') => return OverlayEvent::Action("open:context-defaults".into()),
                KeyCode::Char('g') => return OverlayEvent::Action("open:settings".into()),
                _ => {}
            }
        }
        self.message = None;
        OverlayEvent::None
    }

    pub fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        // A local model's id is its directory; the title names the model, the body keeps the path.
        let name = self
            .profile
            .model
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(&self.profile.model)
            .to_owned();
        panel(
            frame,
            ctx.theme,
            Section::Dial,
            "Model settings",
            &name,
            |width, height| self.compose(width, height, ctx),
        );
    }

    fn value_spans(
        &self,
        field: &Field,
        selected: bool,
        width: usize,
        ctx: &DrawCtx,
    ) -> Vec<Span<'static>> {
        let theme = ctx.theme;
        if selected && let Some(editor) = &self.editor {
            return editor_spans(theme, editor, width, false);
        }
        let value = self.value(field.path);
        let custom = self.is_override(field);
        let shown = match &value {
            Some(v) if field.path == "capabilities" => Self::capability_summary(v),
            Some(v) if custom => Self::show_value(v),
            Some(v) if field.path == "cache_retention" => Self::show_value(v),
            _ => "default".to_owned(),
        };
        let adjustable = !matches!(field.kind, Kind::Toml);
        let (left, right) = if theme.ascii() {
            ("<", ">")
        } else {
            ("‹", "›")
        };
        let text = if selected && adjustable {
            format!("{left} {shown} {right}")
        } else {
            shown
        };
        let style = if selected && adjustable {
            theme.fg(Tok::Accent).add_modifier(Modifier::BOLD)
        } else if custom {
            theme.fg(Tok::Text).add_modifier(Modifier::BOLD)
        } else {
            theme.fg(Tok::Muted)
        };
        vec![Span::styled(
            text::ellipsize(&text, width, theme.ascii()),
            style,
        )]
    }

    fn compose(&mut self, width: u16, height: u16, ctx: &DrawCtx) -> Vec<Line<'static>> {
        let (w, h) = (usize::from(width), usize::from(height));
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let theme = ctx.theme;
        let dim = theme.fg(Tok::Muted);
        let customs = self.fields.iter().filter(|f| self.is_override(f)).count();
        let mut out: Vec<Line<'static>> = Vec::new();
        out.push(Line::from(fit(
            vec![
                Span::styled(
                    format!(" {} {} ", self.profile.provider, theme.sym(Sym::Dot)),
                    theme.fg(Tok::Muted),
                ),
                Span::styled(
                    self.profile.model.clone(),
                    theme.fg(Tok::Accent).add_modifier(Modifier::BOLD),
                ),
            ],
            w,
        )));
        let summary = match customs {
            0 => "using provider defaults".to_owned(),
            1 => "1 custom value".to_owned(),
            n => format!("{n} custom values"),
        };
        out.push(Line::from(fit(
            vec![Span::styled(
                format!(
                    " {} {} saves to {} settings {} {}/{}",
                    summary,
                    theme.sym(Sym::Dot),
                    scope_name(self.scope),
                    theme.sym(Sym::Dot),
                    self.cursor + 1,
                    self.len()
                ),
                dim,
            )],
            w,
        )));

        let detail_h = if h >= 20 {
            5
        } else if h >= 13 {
            3
        } else {
            0
        };
        let list_h = h.saturating_sub(2 + 1 + 1 + detail_h).max(1);

        // Section headers interleaved with fields, scrolled so the selected field stays visible.
        let mut rows: Vec<(Option<&'static str>, usize)> = Vec::new();
        let mut last = "";
        for (i, f) in self.fields.iter().enumerate() {
            if f.section != last {
                rows.push((Some(f.section), i));
                last = f.section;
            }
            rows.push((None, i));
        }
        if !self.suggestions.is_empty() {
            rows.push((Some("Suggestions"), self.fields.len()));
            for k in 0..self.suggestions.len() {
                rows.push((None, self.fields.len() + k));
            }
        }
        let selected_row = rows
            .iter()
            .position(|(s, i)| s.is_none() && *i == self.cursor)
            .unwrap_or(0);
        if selected_row < self.scroll {
            self.scroll = selected_row;
        }
        if selected_row >= self.scroll + list_h {
            self.scroll = selected_row + 1 - list_h;
        }
        if list_h > 1
            && selected_row > 0
            && rows[selected_row - 1].0.is_some()
            && self.scroll >= selected_row
        {
            self.scroll = selected_row - 1;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(list_h));

        let label_w = self
            .fields
            .iter()
            .map(|f| text::width(f.label))
            .max()
            .unwrap_or(10)
            .min(w.saturating_sub(12).max(8));
        let fill = theme.bg(Tok::SelectedBg);
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
            if *index >= self.fields.len() {
                out.push(self.suggestion_row(*index - self.fields.len(), w, ctx, label_w));
                continue;
            }
            let field = self.fields[*index];
            let selected = *index == self.cursor;
            let blocked = self.unsupported(field).is_some();
            let label_style = if blocked {
                theme.fg(Tok::Dim)
            } else {
                theme.fg(Tok::Text)
            };
            let marker = if self.is_override(field) {
                theme.sym(Sym::Dot).to_owned()
            } else {
                " ".to_owned()
            };
            let prefix_w = 5 + label_w + 2;
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
                Span::styled(marker, theme.fg(Tok::Accent)),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{:<label_w$}",
                        text::ellipsize(field.label, label_w, theme.ascii())
                    ),
                    label_style,
                ),
                Span::raw("  "),
            ];
            spans.extend(self.value_spans(field, selected, w.saturating_sub(prefix_w + 1), ctx));
            if blocked && !selected {
                spans.push(Span::styled(" (unsupported)", dim));
            }
            out.push(if selected {
                filled(spans, w, fill)
            } else {
                Line::from(fit(spans, w))
            });
        }
        while out.len() < 2 + list_h {
            out.push(Line::default());
        }

        if detail_h > 0 && self.cursor >= self.fields.len() {
            let mut lines = self.suggestion_detail(self.cursor - self.fields.len(), w, ctx);
            lines.truncate(detail_h);
            while lines.len() < detail_h {
                lines.push(Line::default());
            }
            out.extend(lines);
        } else if detail_h > 0 {
            let field = self.fields[self.cursor];
            let mut lines: Vec<Line<'static>> = vec![Line::from(fit(
                vec![chrome::label(theme, &format!(" {}", field.label))],
                w,
            ))];
            // The title and the range (or the unsupported reason) always keep their rows; help gets the rest.
            let help = text::wrap_words(field.help, w.saturating_sub(2).max(8));
            let help_rows = detail_h.saturating_sub(2);
            for (index, row) in help.iter().take(help_rows).enumerate() {
                let mut shown = format!(" {row}");
                if index + 1 == help_rows && help.len() > help_rows {
                    shown.push_str(&format!(" {}", theme.sym(Sym::Ellipsis)));
                }
                lines.push(Line::from(fit(
                    vec![Span::styled(
                        text::ellipsize(&shown, w, theme.ascii()),
                        theme.fg(Tok::Muted),
                    )],
                    w,
                )));
            }
            let accepts = match field.kind {
                Kind::Number { min, max, int, .. } => format!(
                    "Accepts {} to {}",
                    number_text(min, int),
                    number_text(max, int)
                ),
                Kind::Choice { optional, .. } => format!(
                    "Choose: {}",
                    self.choices(field).join(" · ") + if optional { " · default" } else { "" }
                ),
                Kind::Toggle => "Choose: on · off · default".to_owned(),
                Kind::Toml => "Enter edits a TOML value; blank resets to default".to_owned(),
            };
            match self.unsupported(field) {
                Some(reason) => lines.push(Line::from(fit(
                    vec![Span::styled(
                        format!(" {} {reason}", theme.sym(Sym::Cross2)),
                        theme.fg(Tok::Warning),
                    )],
                    w,
                ))),
                None => lines.push(Line::from(fit(
                    vec![Span::styled(format!(" {accepts}"), dim)],
                    w,
                ))),
            }
            lines.truncate(detail_h);
            while lines.len() < detail_h {
                lines.push(Line::default());
            }
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
            let mut segments = vec![
                format!(
                    "{} move",
                    pair_label(keys, Action::SelectUp, Action::SelectDown)
                ),
                format!("{} change", arrows(theme, Axis::Horizontal)),
                hint(keys, Action::SelectConfirm, "edit"),
                "r reset".to_owned(),
                format!("s save to {}", scope_name(other_scope(self.scope))),
                "d context defaults".to_owned(),
                "g all settings".to_owned(),
                hint(keys, Action::SelectCancel, "close"),
            ];
            segments.retain(|s| !s.is_empty());
            segments
        };
        out.push(hint_line(theme, &segments, w));
        out
    }
}
/// Providers that serve models from this machine or the LAN. Hosted providers never get suggestions.
fn is_local_provider(provider: &str) -> bool {
    matches!(provider, "mlx" | "lm-studio" | "ollama" | "llama-cpp")
}

fn as_number(value: &toml::Value) -> Option<f64> {
    match value {
        toml::Value::Integer(i) => Some(*i as f64),
        toml::Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Integers print bare; floats keep a decimal point so TOML reads them back as floats.
fn number_text(value: f64, int: bool) -> String {
    if int {
        return format!("{}", value.round() as i64);
    }
    let rounded = (value * 10_000.0).round() / 10_000.0;
    let text = format!("{rounded}");
    if text.contains('.') {
        text
    } else {
        format!("{text}.0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::overlay::settings::testkit::{Rig, ch, key, render, screen, status_preview};
    use crossterm::event::KeyCode;

    fn mlx() -> ModelProfile {
        ModelProfile {
            provider: "mlx".into(),
            model: "local-model".into(),
            capabilities: crate::llm::profiles::ModelCapabilities {
                temperature: true,
                top_p: true,
                seed: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn go(overlay: &mut ModelSettingsOverlay, rig: &Rig, path: &str) {
        overlay.cursor = overlay.fields.iter().position(|f| f.path == path).unwrap();
        let _ = rig;
    }

    #[test]
    fn editor_round_trips_local_values_and_rejects_invalid_ones_with_plain_messages() {
        let overlay = ModelSettingsOverlay::new("local".into(), mlx());
        assert_eq!(
            overlay
                .candidate("local_sampling.top_k", "40")
                .unwrap()
                .local_sampling
                .unwrap()
                .top_k,
            Some(40)
        );
        assert!(overlay.candidate("local_sampling.min_p", "1.1").is_err());
        // 0 means "no operator output cap"; values above the documented maximum are refused.
        assert!(overlay.candidate("max_tokens", "0").is_ok());
        assert!(overlay.candidate("max_tokens", "1048577").is_err());
        assert!(
            overlay
                .candidate("tools", "[]\nmodel = 'different'")
                .is_err()
        );
        let message = overlay
            .candidate("temperature", "warm")
            .unwrap_err()
            .to_string();
        assert!(message.contains("quotes"), "{message}");
    }

    #[test]
    fn context_defaults_shortcut_navigates_without_modifying_the_profile() {
        let rig = Rig::new();
        let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
        assert!(matches!(overlay.event(&ch('d'), &rig.keys),
            OverlayEvent::Action(action) if action == "open:context-defaults"));
        assert!(overlay.profile.context_window.is_none());
        assert!(overlay.pending.is_none());
    }

    #[test]
    fn declared_capabilities_read_as_plain_words_and_keep_the_raw_toml() {
        let overlay = ModelSettingsOverlay::new("local".into(), mlx());
        let table = overlay.table();
        let caps = table.get("capabilities").expect("capabilities table");
        let summary = ModelSettingsOverlay::capability_summary(caps);
        assert!(summary.contains("Temperature: yes"), "{summary}");
        assert!(summary.contains("Seed: yes"), "{summary}");
        assert!(summary.contains("Image input: not declared"), "{summary}");
        assert!(!summary.contains(" = "), "raw TOML leaked: {summary}");
    }

    #[test]
    fn arrows_start_at_a_sensible_value_then_step_and_clamp() {
        let rig = Rig::new();
        let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
        go(&mut overlay, &rig, "temperature");
        for (event, expected) in [
            (KeyCode::Right, 0.2),
            (KeyCode::Right, 0.25),
            (KeyCode::Left, 0.2),
        ] {
            let OverlayEvent::Set { value, .. } = overlay.event(&key(event), &rig.keys) else {
                panic!("expected a save")
            };
            assert_eq!(value["temperature"].as_float(), Some(expected));
            overlay.settled(Ok(()));
        }
        assert!(overlay.is_override(overlay.fields[overlay.cursor]));
        assert!(
            matches!(overlay.event(&ch('r'), &rig.keys), OverlayEvent::Set { value, .. } if value.get("temperature").is_none())
        );
        overlay.settled(Ok(()));
        assert!(!overlay.is_override(overlay.fields[overlay.cursor]));
    }

    #[test]
    fn context_size_saves_validates_and_resets_for_local_and_cloud_models() {
        let rig = Rig::new();
        for provider in ["mlx", "openai", "lm-studio"] {
            let mut profile = mlx();
            profile.provider = provider.into();
            let mut overlay = ModelSettingsOverlay::new("model".into(), profile);
            go(&mut overlay, &rig, "context_window");
            assert!(matches!(
                overlay.apply("65536"),
                OverlayEvent::Set { value, .. } if value["context_window"].as_integer() == Some(65536)
            ));
            overlay.settled(Ok(()));
            assert_eq!(overlay.profile.context_window, Some(65536));
            let encoded = toml::to_string(&overlay.profile).unwrap();
            let decoded: ModelProfile = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.context_window, Some(65536));
            for invalid in ["0", "2047", "2097153", "32768.5", "-1"] {
                assert!(overlay.candidate("context_window", invalid).is_err());
            }
            assert!(matches!(
                overlay.event(&ch('r'), &rig.keys),
                OverlayEvent::Set { value, .. } if value.get("context_window").is_none()
            ));
            overlay.settled(Ok(()));
            assert_eq!(overlay.profile.context_window, None);
            assert!(matches!(
                overlay.event(&key(KeyCode::Right), &rig.keys),
                OverlayEvent::Set { value, .. } if value["context_window"].as_integer() == Some(32768)
            ));
        }
    }

    #[test]
    fn choices_toggles_and_typed_numbers_save_valid_profiles() {
        let rig = Rig::new();
        let mut profile = mlx();
        profile.provider = "mlx".into();
        let mut overlay = ModelSettingsOverlay::new("local".into(), profile);
        go(&mut overlay, &rig, "local_sampling.top_k");
        overlay.event(&key(KeyCode::Enter), &rig.keys);
        assert!(overlay.editor.is_some());
        for c in "25".chars() {
            overlay.event(&ch(c), &rig.keys);
        }
        assert!(
            matches!(overlay.event(&key(KeyCode::Enter), &rig.keys), OverlayEvent::Set { value, .. } if value["local_sampling"]["top_k"].as_integer() == Some(25))
        );
        overlay.settled(Ok(()));
        assert!(overlay.editor.is_none());
        go(&mut overlay, &rig, "local_sampling.min_p");
        overlay.event(&key(KeyCode::Enter), &rig.keys);
        for c in "7".chars() {
            overlay.event(&ch(c), &rig.keys);
        }
        assert!(matches!(
            overlay.event(&key(KeyCode::Enter), &rig.keys),
            OverlayEvent::None
        ));
        assert!(
            overlay.editor.is_some(),
            "an invalid value keeps the editor open"
        );
        assert!(overlay.message.as_ref().is_some_and(|(_, error)| *error));
    }

    #[test]
    fn page_shows_sections_values_and_help_and_survives_tiny_terminals() {
        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let mut profile = mlx();
        profile.temperature = Some(0.3);
        let mut overlay = ModelSettingsOverlay::new("local".into(), profile);
        let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
        assert!(
            text.contains("Context size") && text.contains("2048 to 2097152"),
            "{text}"
        );
        go(&mut overlay, &rig, "temperature");
        let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
        for needle in [
            "\u{25c6} Model settings",
            "SAMPLING",
            "Temperature",
            "0.3",
            "default",
            "custom value",
            "Randomness",
            "reset",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        go(&mut overlay, &rig, "local_sampling.chat_template_kwargs");
        let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
        assert!(
            text.contains("Template arguments") && text.to_lowercase().contains("chat template"),
            "{text}"
        );
        if std::env::var_os("DIAL_RENDER_MODEL_SETTINGS").is_some() {
            std::fs::create_dir_all("target/ui-previews").unwrap();
            std::fs::write("target/ui-previews/model-settings.txt", &text).unwrap();
        }
        for (width, height) in [(1, 1), (10, 3), (24, 8), (50, 15), (80, 24), (160, 50)] {
            let buffer = render(width, height, |frame| overlay.draw(frame, &ctx));
            assert_eq!(buffer.area.width, width);
        }
    }

    #[test]
    fn detail_box_keeps_the_range_line_when_help_wraps() {
        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
        go(&mut overlay, &rig, "context_window");
        for (width, height) in [(100, 32), (60, 32)] {
            let text = screen(&render(width, height, |frame| overlay.draw(frame, &ctx)));
            assert!(text.contains("Accepts 2048 to"), "{width}x{height}: {text}");
        }
        let ascii = crate::ui::theme::Theme::dark().with_glyphs(crate::ui::theme::Glyphs::Ascii);
        let ascii_ctx = DrawCtx {
            theme: &ascii,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ascii_ctx)));
        assert!(text.contains("<-/->") && !text.contains('←'), "{text}");
    }

    #[test]
    fn unsupported_fields_explain_how_to_enable_them_and_cloud_pages_hide_local_ones() {
        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let mut cloud = ModelSettingsOverlay::new(
            "cloud".into(),
            ModelProfile {
                provider: "anthropic".into(),
                model: "cloud-model".into(),
                ..Default::default()
            },
        );
        assert!(
            !cloud
                .fields
                .iter()
                .any(|f| f.path.starts_with("local_sampling"))
        );
        assert!(cloud.fields.iter().any(|f| f.path == "prompt_cache"));
        go(&mut cloud, &rig, "temperature");
        let text = screen(&render(100, 32, |frame| cloud.draw(frame, &ctx)));
        assert!(text.contains("Not declared as supported"), "{text}");
        let local = ModelSettingsOverlay::new("local".into(), mlx());
        assert!(
            !local
                .fields
                .iter()
                .any(|f| f.path == "prompt_cache" || f.path == "thinking")
        );
    }
    #[test]
    fn lmstudio_settings_expose_supported_local_controls() {
        let rig = Rig::new();
        let mut profile = mlx();
        profile.provider = "lm-studio".into();
        let mut overlay = ModelSettingsOverlay::new("LM Studio".into(), profile);
        for path in [
            "local_sampling.top_k",
            "local_sampling.min_p",
            "local_sampling.seed",
            "local_sampling.repetition_penalty",
            "local_sampling.presence_penalty",
            "local_sampling.frequency_penalty",
        ] {
            assert!(
                overlay.fields.iter().any(|f| f.path == path),
                "missing {path}"
            );
        }
        for path in [
            "local_sampling.xtc_probability",
            "local_sampling.repetition_context_size",
            "local_sampling.chat_template_kwargs",
            "prompt_cache",
        ] {
            assert!(
                !overlay.fields.iter().any(|f| f.path == path),
                "unsupported {path}"
            );
        }
        go(&mut overlay, &rig, "local_sampling.top_k");
        let candidate = overlay.candidate("local_sampling.top_k", "40").unwrap();
        assert_eq!(candidate.local_sampling.unwrap().top_k, Some(40));
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
        assert!(text.contains("Top-k"), "{text}");
    }

    #[test]
    fn a_failed_save_keeps_the_editor_and_shows_the_reason() {
        let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
        let mut editor = Editor::default();
        editor.set("1000");
        overlay.editor = Some(editor);
        overlay.pending = Some((overlay.profile.clone(), "saved".into()));
        overlay.settled(Err("cannot write settings\nmore detail".into()));
        assert!(overlay.editor.is_some() && overlay.pending.is_none());
        assert_eq!(
            overlay.message,
            Some(("cannot write settings".into(), true))
        );
    }

    #[test]
    fn suggestions_are_listed_and_enter_applies_only_the_selected_one() {
        let rig = Rig::new();
        let ctx = DrawCtx {
            theme: &rig.theme,
            keys: &rig.keys,
            status: &rig.status,
            preview_status: &status_preview,
        };
        let mut overlay = ModelSettingsOverlay::new("local".into(), mlx());
        overlay.suggestions = vec![
            Suggestion {
                path: "edit_mode".into(),
                global: false,
                value: serde_json::json!("apply_patch"),
                title: "Use the apply_patch edit grammar".into(),
                evidence: "edits failed 9 of 20 times under replace, 1 of 15 under apply_patch"
                    .into(),
            },
            Suggestion {
                path: "assistance.local.side_models".into(),
                global: true,
                value: serde_json::json!("reuse_main"),
                title: "Reuse the main model for side requests".into(),
                evidence: "side requests swapped the loaded model 4 times".into(),
            },
        ];
        overlay.cursor = overlay.fields.len();
        let text = screen(&render(100, 32, |frame| overlay.draw(frame, &ctx)));
        assert!(
            // Long titles are cut to the label column; the detail box shows the whole title.
            text.contains("SUGGESTIONS") && text.contains("Use the apply_patch"),
            "{text}"
        );
        assert!(text.contains("edits failed 9 of 20"), "{text}");

        // Only the selected row is saved, and only once Enter is pressed.
        overlay.cursor = overlay.fields.len();
        assert!(matches!(
            overlay.event(&key(KeyCode::Right), &rig.keys),
            OverlayEvent::None
        ));
        let OverlayEvent::Set { path, value, .. } = overlay.event(&key(KeyCode::Enter), &rig.keys)
        else {
            panic!("expected a profile save")
        };
        assert_eq!(path, "model_profiles.local");
        assert_eq!(value["edit_mode"].as_str(), Some("apply_patch"));
        assert_eq!(overlay.suggestions.len(), 2, "still listed until saved");
        overlay.settled(Ok(()));
        assert_eq!(overlay.suggestions.len(), 1);
        assert_eq!(
            overlay.profile.edit_mode,
            Some(crate::config::types::EditMode::ApplyPatch)
        );

        // A global key goes through the config path, and a failed save keeps the row.
        overlay.cursor = overlay.fields.len();
        let OverlayEvent::Set { path, value, .. } = overlay.event(&key(KeyCode::Enter), &rig.keys)
        else {
            panic!("expected a config save")
        };
        assert_eq!(path, "assistance.local.side_models");
        assert_eq!(value.as_str(), Some("reuse_main"));
        overlay.settled(Err("cannot write settings".into()));
        assert_eq!(overlay.suggestions.len(), 1);
        assert!(matches!(
            overlay.event(&key(KeyCode::Enter), &rig.keys),
            OverlayEvent::Set { .. }
        ));
        overlay.settled(Ok(()));
        assert!(overlay.suggestions.is_empty());
        assert_eq!(overlay.cursor, overlay.fields.len() - 1);
    }

    #[test]
    fn evidence_file_offers_suggestions_for_local_models_only() {
        let dir = tempfile::tempdir().unwrap();
        let mlx_key = local_evidence::model_key("mlx", "local-model");
        let hosted_key = local_evidence::model_key("anthropic", "local-model");
        let file = serde_json::json!({
            mlx_key: {
                "stage": 2,
                "edits": {
                    "replace": {"ok": 1, "failed": 9},
                    "apply_patch": {"ok": 14, "failed": 1}
                }
            },
            hosted_key: {
                "stage": 2,
                "edits": {"replace": {"ok": 1, "failed": 9}}
            }
        });
        std::fs::write(dir.path().join(local_evidence::FILE), file.to_string()).unwrap();
        let base = Current {
            edit_mode: "replace".into(),
            side_models: "warn".into(),
            context_window: 32768,
            ..Current::default()
        };
        let overlay = ModelSettingsOverlay::new("local".into(), mlx())
            .with_suggestions(dir.path(), base.clone());
        assert!(
            overlay
                .suggestions
                .iter()
                .any(|s| s.path == "edit_mode" && s.value == serde_json::json!("apply_patch")),
            "{:?}",
            overlay.suggestions
        );
        let mut hosted = mlx();
        hosted.provider = "anthropic".into();
        let hosted =
            ModelSettingsOverlay::new("cloud".into(), hosted).with_suggestions(dir.path(), base);
        assert!(hosted.suggestions.is_empty());
    }
}

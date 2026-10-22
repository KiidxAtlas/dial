//! `/model settings`: a profile editor for the current main model.
//!
//! Values are grouped into sections. Toggles and choices cycle with Left/Right, numbers step the same
//! way (Enter types an exact value), text rows take plain typed text, and anything else is a TOML
//! literal. Local models add rows read from their own files: each option the chat template accepts,
//! and the installed draft models that can speed up an MLX model. Every change is validated against
//! the provider capabilities and saved immediately; an unset value follows the provider default.
use super::{
    DrawCtx, OverlayEvent,
    settings::{
        Axis, arrows, editor_spans, filled, fit, hint, hint_line, other_scope, pair_label, panel,
        pressed, scope_name,
    },
};
use crate::{
    config::settings_store::Scope,
    llm::{
        local_evidence::{self, Current, Suggestion},
        local_server,
        profiles::{ModelProfile, ReasoningApi},
        template_options::{self, OptionKind, TemplateOption},
    },
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
use std::{borrow::Cow, path::Path};

#[derive(Clone)]
enum Kind {
    Number {
        min: f64,
        max: f64,
        step: f64,
        start: f64,
        int: bool,
    },
    /// Fixed options; `bare` options are numbers and are saved without quotes.
    Choice {
        options: &'static [&'static str],
        optional: bool,
        bare: bool,
    },
    /// Options found at runtime (template literals, installed draft models); always optional text.
    Found(Vec<String>),
    Toggle,
    /// Plain text: typed without quotes and saved as a string.
    Text,
    Toml,
}

#[derive(Clone)]
struct Field {
    path: Cow<'static, str>,
    label: Cow<'static, str>,
    section: &'static str,
    kind: Kind,
    help: Cow<'static, str>,
}

const fn field(
    path: &'static str,
    label: &'static str,
    section: &'static str,
    kind: Kind,
    help: &'static str,
) -> Field {
    Field {
        path: Cow::Borrowed(path),
        label: Cow::Borrowed(label),
        section,
        kind,
        help: Cow::Borrowed(help),
    }
}

const fn choice(options: &'static [&'static str], optional: bool) -> Kind {
    Kind::Choice {
        options,
        optional,
        bare: false,
    }
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

/// The `chat_template_kwargs` table every template row writes into.
const TEMPLATE_KWARGS: &str = "local_sampling.chat_template_kwargs";
const DRAFT_MODEL: &str = "local_runtime.draft_model";

static FIELDS: &[Field] = &[
    field(
        "context_window",
        "Context size",
        "Context",
        int(2048.0, 2_097_152.0, 1024.0, 32768.0),
        "This model's history/compaction cap, limited by model/server capacity. Reset follows shared defaults. Press d to change the local/cloud defaults for future model selections; this does not resize an external server.",
    ),
    field(
        "temperature",
        "Temperature",
        "SAMPLING",
        float(0.0, 2.0, 0.05, 0.2),
        "Randomness of the output. Lower is more deterministic and usually better for coding.",
    ),
    field(
        "top_p",
        "Top-p",
        "SAMPLING",
        float(0.0, 1.0, 0.05, 0.95),
        "Nucleus sampling: only the most likely tokens whose probabilities add up to this value are considered.",
    ),
    field(
        "max_tokens",
        "Max output tokens",
        "Output",
        int(1.0, 1_048_576.0, 1024.0, 8192.0),
        "Upper bound on tokens generated per model response, including reasoning where the provider counts it.",
    ),
    field(
        "reasoning_effort",
        "Reasoning effort",
        "Reasoning",
        choice(EFFORTS, true),
        "How much the model thinks before answering. Higher costs more time and tokens.",
    ),
    field(
        "thinking",
        "Thinking",
        "Reasoning",
        Kind::Toggle,
        "Force extended thinking on or off. Default lets the provider and reasoning effort decide.",
    ),
    field(
        "thinking_budget",
        "Thinking budget",
        "Reasoning",
        int(1024.0, 1_048_576.0, 1024.0, 4096.0),
        "Token budget for Anthropic-style extended thinking. Must stay below the output limit.",
    ),
    field(
        "prompt_cache",
        "Prompt caching",
        "Caching",
        Kind::Toggle,
        "Reuse the stable prompt prefix between requests to cut cost and latency. Default is automatic.",
    ),
    field(
        "cache_retention",
        "Cache lifetime",
        "Caching",
        choice(&["auto", "short", "long"], false),
        "How long cached prompt prefixes live. Auto uses 1 hour for subscription sign-in and 5 minutes otherwise.",
    ),
    field(
        "edit_mode",
        "Edit style",
        "Agent behavior",
        choice(
            &["hashline", "replace", "apply_patch", "patch", "sloppy"],
            true,
        ),
        "Pin this exact model's edit grammar: replace, hashline, apply_patch, structured patch or sloppy matching.",
    ),
    field(
        "tools",
        "Tool allow-list",
        "Agent behavior",
        Kind::Toml,
        "Limit which tools this model sees, e.g. [\"read\", \"grep\", \"edit\"]. Blank offers the normal set.",
    ),
    field(
        "local_sampling.top_k",
        "Top-k",
        "Local sampling",
        int(0.0, 1000.0, 5.0, 40.0),
        "Only sample from the k most likely tokens. 0 disables the limit.",
    ),
    field(
        "local_sampling.min_p",
        "Min-p",
        "Local sampling",
        float(0.0, 1.0, 0.01, 0.05),
        "Drop tokens less likely than this fraction of the best token. A good alternative to top-p.",
    ),
    field(
        "local_sampling.seed",
        "Seed",
        "Local sampling",
        int(0.0, 4_294_967_295.0, 1.0, 0.0),
        "Fixed random seed for repeatable output. Needs the seed capability to be declared.",
    ),
    field(
        "local_sampling.xtc_probability",
        "XTC probability",
        "Local sampling",
        float(0.0, 1.0, 0.05, 0.1),
        "Chance of excluding top choices to increase variety. 0 turns XTC off.",
    ),
    field(
        "local_sampling.xtc_threshold",
        "XTC threshold",
        "Local sampling",
        float(0.0, 1.0, 0.05, 0.1),
        "Probability above which a token may be excluded by XTC.",
    ),
    field(
        "local_sampling.repetition_penalty",
        "Repetition penalty",
        "Repetition",
        float(0.0, 3.0, 0.05, 1.1),
        "Discourage repeating recent tokens. 1.0 is neutral; slightly above 1 reduces loops.",
    ),
    field(
        "local_sampling.repetition_context_size",
        "Repetition window",
        "Repetition",
        int(0.0, 8192.0, 16.0, 64.0),
        "How many recent tokens the repetition penalty looks back over.",
    ),
    field(
        "local_sampling.presence_penalty",
        "Presence penalty",
        "Repetition",
        float(-2.0, 2.0, 0.1, 0.0),
        "Penalize any token that already appeared, encouraging new topics.",
    ),
    field(
        "local_sampling.presence_context_size",
        "Presence window",
        "Repetition",
        int(0.0, 8192.0, 16.0, 64.0),
        "How many recent tokens the presence penalty considers.",
    ),
    field(
        "local_sampling.frequency_penalty",
        "Frequency penalty",
        "Repetition",
        float(-2.0, 2.0, 0.1, 0.0),
        "Penalize tokens in proportion to how often they appeared.",
    ),
    field(
        "local_sampling.frequency_context_size",
        "Frequency window",
        "Repetition",
        int(0.0, 8192.0, 16.0, 64.0),
        "How many recent tokens the frequency penalty considers.",
    ),
    field(
        TEMPLATE_KWARGS,
        "All template arguments",
        "Chat template",
        Kind::Toml,
        "Every chat template argument as one TOML table, e.g. { enable_thinking = false }. Options this model's template reads are listed above it on their own rows.",
    ),
    field(
        DRAFT_MODEL,
        "Draft model",
        "Local runtime",
        Kind::Text,
        "A smaller model with the same tokenizer that drafts tokens for speculative decoding. Enter types a model id or path. Any value on this page replaces the global local_runtime for this model; Dial restarts its MLX server to apply it.",
    ),
    field(
        "local_runtime.num_draft_tokens",
        "Draft tokens",
        "Local runtime",
        int(1.0, 16.0, 1.0, 3.0),
        "Tokens the draft model proposes per step. Needs a draft model; default 3.",
    ),
    field(
        "local_runtime.kv_bits",
        "KV cache bits",
        "Local runtime",
        Kind::Choice {
            options: &["2", "3", "4", "6", "8"],
            optional: true,
            bare: true,
        },
        "Quantize the KV cache to this many bits: less memory for long contexts, slightly lower quality. Default keeps it unquantized.",
    ),
    field(
        "local_runtime.kv_group_size",
        "KV group size",
        "Local runtime",
        Kind::Choice {
            options: &["32", "64", "128"],
            optional: true,
            bare: true,
        },
        "Quantization group size for the KV cache. Needs KV cache bits.",
    ),
    field(
        "local_runtime.quantized_kv_start",
        "Quantize KV after",
        "Local runtime",
        int(0.0, 1_048_576.0, 1024.0, 4096.0),
        "Keep the first this-many tokens of the KV cache at full precision. Needs KV cache bits.",
    ),
    field(
        "keep_alive",
        "Keep loaded",
        "Ollama",
        choice(&["5m", "30m", "1h", "4h", "-1"], true),
        "How long Ollama keeps the model in memory after a request; -1 keeps it loaded. Default is Ollama's own.",
    ),
    field(
        "capabilities",
        "Declared capabilities",
        "Advanced",
        Kind::Toml,
        "What this exact model supports, one line per capability. Settings are validated against it. Enter opens the exact values as TOML.",
    ),
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
        "Chat template" | "Local runtime" => mlx,
        "Ollama" => profile.provider == "ollama",
        "Caching" => !mlx && profile.provider != "lm-studio",
        "Reasoning" => !mlx || profile.capabilities.reasoning != ReasoningApi::None,
        _ => true,
    }
}

/// One row for an option the model's chat template reads, saved under `chat_template_kwargs`.
fn template_row(option: TemplateOption) -> Field {
    let (kind, what) = match option.kind {
        OptionKind::Flag => (Kind::Toggle, "an on/off switch"),
        OptionKind::Choice(options) => (Kind::Found(options), "one of the values it compares"),
        OptionKind::Text => (Kind::Text, "a value; Enter types it"),
    };
    let default = option.default.map_or_else(
        || "Default lets the template decide.".to_owned(),
        |value| format!("The template's own default is {value}."),
    );
    Field {
        path: Cow::Owned(format!("{TEMPLATE_KWARGS}.{}", option.name)),
        label: Cow::Owned(option.name.clone()),
        section: "Chat template",
        kind,
        help: Cow::Owned(format!(
            "This model's chat template reads `{}` as {what}. {default} Sent as chat_template_kwargs.{} with every request.",
            option.name, option.name
        )),
    }
}

pub struct ModelSettingsOverlay {
    pub name: String,
    profile: ModelProfile,
    fields: Vec<Field>,
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
        let fields = FIELDS
            .iter()
            .filter(|f| shown(f, &profile))
            .cloned()
            .collect();
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

    /// Local models only: adds the rows this model's own files offer. Each option its chat template
    /// reads gets a row before the raw template-arguments table, and an MLX model's draft-model row
    /// cycles through the installed models that share its tokenizer. Reads files; never loads weights.
    pub fn with_model_files(mut self) -> Self {
        let options = template_options::detect(&self.profile.provider, &self.profile.model);
        let drafts = if self.profile.provider == "mlx" {
            local_server::draft_candidates(&self.profile.model)
        } else {
            Vec::new()
        };
        self.add_model_rows(options, drafts);
        self
    }

    fn add_model_rows(&mut self, options: Vec<TemplateOption>, drafts: Vec<String>) {
        if !drafts.is_empty()
            && let Some(row) = self.fields.iter_mut().find(|f| f.path == DRAFT_MODEL)
        {
            row.kind = Kind::Found(drafts);
            row.help = Cow::Borrowed(
                "A smaller installed model with the same tokenizer drafts tokens for speculative decoding; Left/Right picks one. Any value on this page replaces the global local_runtime for this model; Dial restarts its MLX server to apply it.",
            );
        }
        let Some(at) = self.fields.iter().position(|f| f.path == TEMPLATE_KWARGS) else {
            return;
        };
        self.fields
            .splice(at..at, options.into_iter().map(template_row));
    }

    fn table(&self) -> toml::Table {
        toml::Value::try_from(&self.profile)
            .expect("serializable model profile")
            .as_table()
            .unwrap()
            .clone()
    }

    fn value(&self, path: &str) -> Option<toml::Value> {
        crate::config::schema::get(&self.table(), path).cloned()
    }

    fn is_override(&self, field: &Field) -> bool {
        match field.path.as_ref() {
            "capabilities" => false,
            "cache_retention" => self
                .value(&field.path)
                .is_some_and(|v| v.as_str() != Some("auto")),
            path => self.value(path).is_some(),
        }
    }

    fn unsupported(&self, field: &Field) -> Option<&'static str> {
        let c = &self.profile.capabilities;
        match field.path.as_ref() {
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
        match &field.kind {
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
            Kind::Found(options) => options.clone(),
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
                crate::config::schema::set(&mut table, field, value);
            }
            None => {
                crate::config::schema::unset(&mut table, field);
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
        let field = self.fields[self.cursor].clone();
        match self.candidate(&field.path, text) {
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
        let field = self.fields[self.cursor].clone();
        let current = self.value(&field.path);
        let literal = match field.kind {
            Kind::Toggle => {
                let options = [None, Some(true), Some(false)];
                let at = options
                    .iter()
                    .position(|o| *o == current.as_ref().and_then(toml::Value::as_bool))
                    .unwrap_or(0) as i64;
                options[(at + direction).rem_euclid(3) as usize].map(|b| b.to_string())
            }
            Kind::Choice { optional, bare, .. } => {
                self.cycle(&field, current.as_ref(), optional, bare, direction)
            }
            Kind::Found(_) => self.cycle(&field, current.as_ref(), true, false, direction),
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
            Kind::Toml | Kind::Text => return OverlayEvent::None,
        };
        self.apply(&literal.unwrap_or_default())
    }

    /// The next option after `current` as a TOML literal; `None` is the default slot of an
    /// optional choice. A current value outside the list starts from the beginning.
    fn cycle(
        &self,
        field: &Field,
        current: Option<&toml::Value>,
        optional: bool,
        bare: bool,
        direction: i64,
    ) -> Option<String> {
        let list = self.choices(field);
        let lead = i64::from(optional);
        let at = current
            .map(|value| match value {
                toml::Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .and_then(|text| list.iter().position(|option| *option == text))
            .map_or(0, |i| i as i64 + lead);
        let next = (at + direction).rem_euclid(list.len() as i64 + lead);
        (next >= lead).then(|| {
            let option = &list[(next - lead) as usize];
            if bare {
                option.clone()
            } else {
                toml::Value::String(option.clone()).to_string()
            }
        })
    }

    fn begin_edit(&mut self) -> OverlayEvent {
        let value = self.value(&self.fields[self.cursor].path);
        let shown = match self.fields[self.cursor].kind {
            Kind::Number { .. } | Kind::Toml => value.map(|v| v.to_string()),
            // Text rows, and found options where a value outside the list can still be typed.
            Kind::Text | Kind::Found(_) => value.map(|v| match v {
                toml::Value::String(text) => text,
                other => other.to_string(),
            }),
            _ => return self.step(1),
        };
        let mut editor = Editor::default();
        editor.set(&shown.unwrap_or_default());
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
                let text = editor.text.clone();
                // Text rows take what was typed; save it as a TOML string unless it is blank (reset).
                let plain = matches!(self.fields[self.cursor].kind, Kind::Text | Kind::Found(_));
                if plain && !text.trim().is_empty() {
                    return self.apply(&toml::Value::String(text.trim().to_owned()).to_string());
                }
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
                    if self.cursor < self.fields.len()
                        && self.is_override(&self.fields[self.cursor])
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
        let value = self.value(&field.path);
        let custom = self.is_override(field);
        let shown = match &value {
            Some(v) if field.path == "capabilities" => Self::capability_summary(v),
            Some(v) if custom => Self::show_value(v),
            Some(v) if field.path == "cache_retention" => Self::show_value(v),
            _ => "default".to_owned(),
        };
        let adjustable = !matches!(field.kind, Kind::Toml | Kind::Text);
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
            .map(|f| text::width(&f.label))
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
            let field = &self.fields[*index];
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
                        text::ellipsize(&field.label, label_w, theme.ascii())
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
            let field = &self.fields[self.cursor];
            let mut lines: Vec<Line<'static>> = vec![Line::from(fit(
                vec![chrome::label(theme, &format!(" {}", field.label))],
                w,
            ))];
            // The title and the range (or the unsupported reason) always keep their rows; help gets the rest.
            let help = text::wrap_words(&field.help, w.saturating_sub(2).max(8));
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
                Kind::Found(_) => format!(
                    "Choose: {} · default; Enter types another value",
                    self.choices(field).join(" · ")
                ),
                Kind::Toggle => "Choose: on · off · default".to_owned(),
                Kind::Text => "Enter types a value; blank resets to default".to_owned(),
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
mod tests;

//! Persisted UI preferences and provider-independent model selections.
use serde::{Deserialize, Serialize};

/// Shared budget ceiling, large enough for decimal 1.05M and 2M model windows.
pub const MAX_CONTEXT_WINDOW: u32 = 2_097_152;

/// Wire protocol for operator-configured endpoints. Omission preserves the
/// provider's built-in protocol; custom providers otherwise use Chat Completions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ApiProtocol {
    ChatCompletions,
    Responses,
    Anthropic,
    Gemini,
    Ollama,
    /// AWS Bedrock Converse streaming, SigV4 or bearer-token signed.
    Bedrock,
    /// Gemini on Google Vertex AI (OAuth/ADC bearer).
    Vertex,
    /// Anthropic Claude on Google Vertex AI (`streamRawPredict`).
    VertexAnthropic,
    /// Azure OpenAI Responses (`api-key` or Entra bearer).
    AzureResponses,
    /// GitHub Copilot chat surface (OAuth).
    Copilot,
    /// Google Gemini CLI Cloud Code Assist.
    CloudCodeAssist,
    /// Google Antigravity's agent request protocol over Cloud Code Assist.
    Antigravity,
    /// GitLab Duo AI-gateway proxy (OAuth or GITLAB_TOKEN).
    GitlabDuo,
    FactoryDroid,
    /// Devin's Cascade Connect/protobuf streaming protocol (OAuth or session token).
    DevinAgent,
    /// Cursor's native agent protocol: Connect streaming plus a BidiAppend uplink (OAuth).
    Cursor,
    /// GitLab Duo Agent Platform: workflow sessions over WebSocket (OAuth or GITLAB_TOKEN).
    GitlabDuoAgent,
}
impl ApiProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat-completions",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::Ollama => "ollama",
            Self::Bedrock => "bedrock",
            Self::Vertex => "vertex",
            Self::VertexAnthropic => "vertex-anthropic",
            Self::AzureResponses => "azure-responses",
            Self::Copilot => "copilot",
            Self::CloudCodeAssist => "cloud-code-assist",
            Self::Antigravity => "antigravity",
            Self::GitlabDuo => "gitlab-duo",
            Self::FactoryDroid => "factory-droid",
            Self::DevinAgent => "devin-agent",
            Self::Cursor => "cursor",
            Self::GitlabDuoAgent => "gitlab-duo-agent",
        }
    }
    pub fn builtin(provider: &str) -> Self {
        super::policy::protocol(provider)
    }

    /// Wire protocols whose request builders emit `parallel_tool_calls`; Gemini and Ollama reject the option.
    pub fn accepts_parallel_tool_calls(self) -> bool {
        matches!(
            self,
            Self::ChatCompletions | Self::Responses | Self::Anthropic | Self::AzureResponses
        )
    }
}

/// Explicitly nonsecret literals or names of secret environment variables.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HeaderValue {
    Literal(String),
    Env(String),
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequestHeader {
    pub name: String,
    pub value: HeaderValue,
}
pub type RequestHeaders = Vec<RequestHeader>;

pub fn deserialize_headers<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<RequestHeaders>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Rows(RequestHeaders),
        Inherit(String),
    }
    match Option::<Input>::deserialize(d)? {
        None => Ok(None),
        Some(Input::Inherit(value)) if value == "inherit" => Ok(None),
        Some(Input::Rows(rows)) => {
            validate_headers(&rows).map_err(serde::de::Error::custom)?;
            Ok(Some(rows))
        }
        _ => Err(serde::de::Error::custom(
            "headers must be an array or \"inherit\"",
        )),
    }
}
pub fn headers_setting(headers: &Option<RequestHeaders>) -> toml::Value {
    match headers {
        None => toml::Value::String("inherit".into()),
        Some(rows) => toml::Value::Array(
            rows.iter()
                .map(|row| {
                    let (kind, value) = match &row.value {
                        HeaderValue::Literal(value) => ("literal", value),
                        HeaderValue::Env(name) => ("env", name),
                    };
                    let mut source = toml::map::Map::new();
                    source.insert(kind.into(), toml::Value::String(value.clone()));
                    let mut header = toml::map::Map::new();
                    header.insert("name".into(), toml::Value::String(row.name.clone()));
                    header.insert("value".into(), toml::Value::Table(source));
                    toml::Value::Table(header)
                })
                .collect(),
        ),
    }
}

pub fn validate_headers(headers: &[RequestHeader]) -> anyhow::Result<()> {
    use anyhow::ensure;
    ensure!(
        headers.len() <= 32,
        "at most 32 request headers are allowed"
    );
    let mut names = std::collections::BTreeSet::new();
    let mut bytes = 0;
    for row in headers {
        ensure!(
            !row.name.is_empty()
                && row.name.len() <= 128
                && row
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
            "request header name is invalid"
        );
        let name = row.name.to_ascii_lowercase();
        ensure!(
            !matches!(
                name.as_str(),
                "authorization"
                    | "proxy-authorization"
                    | "host"
                    | "content-length"
                    | "content-type"
                    | "accept"
                    | "connection"
                    | "transfer-encoding"
                    | "cookie"
                    | "set-cookie"
                    | "x-api-key"
                    | "api-key"
                    | "x-goog-api-key"
                    | "anthropic-version"
                    | "anthropic-beta"
                    | "openai-beta"
                    | "openai-organization"
                    | "openai-project"
                    | "chatgpt-account-id"
                    | "originator"
                    | "version"
                    | "x-codex-routing-hint"
            ) && !name.starts_with("x-stainless-")
                && !name.contains("account")
                && !name.contains("tenant"),
            "request header is reserved for transport, provider authentication or account routing"
        );
        ensure!(names.insert(name), "duplicate request header name");
        let value = match &row.value {
            HeaderValue::Literal(value) => {
                validate_header_value(value)?;
                value
            }
            HeaderValue::Env(name) => {
                ensure!(
                    !name.is_empty()
                        && name.len() <= 128
                        && name
                            .bytes()
                            .next()
                            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "request header environment variable name is invalid"
                );
                name
            }
        };
        bytes += row.name.len() + value.len();
    }
    ensure!(
        bytes <= 16_384,
        "request headers exceed 16384 configured bytes"
    );
    Ok(())
}
fn validate_header_value(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= 4096 && value.bytes().all(|b| (b' '..=b'~').contains(&b)),
        "request header value must be at most 4096 printable ASCII bytes"
    );
    Ok(())
}
pub(super) fn private_header_value(value: &str) -> anyhow::Result<ureq::http::HeaderValue> {
    let mut field = ureq::http::HeaderValue::from_bytes(value.as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid request header value"))?;
    field.set_sensitive(true);
    Ok(field)
}

/// Opaque native-history identity. Secret material feeds the digest directly,
/// never a JSON value, copied buffer, cache key or diagnostic.
pub(super) fn native_replay_scope(
    domain: &str,
    url: &str,
    model: Option<&str>,
    stable_account: Option<&str>,
    credential: Option<&str>,
    headers: &[(&str, &str)],
) -> String {
    use sha2::{Digest, Sha256};
    fn field(hash: &mut Sha256, value: &str) {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    fn optional(hash: &mut Sha256, value: Option<&str>) {
        match value {
            Some(value) => {
                hash.update([1]);
                field(hash, value);
            }
            None => hash.update([0]),
        }
    }
    let mut hash = Sha256::new();
    field(&mut hash, "dial-native-replay-v2");
    field(&mut hash, domain);
    field(&mut hash, url);
    optional(&mut hash, model);
    optional(&mut hash, stable_account);
    // OAuth refresh does not change a pinned account's replay ownership.
    if stable_account.is_none() {
        optional(&mut hash, credential);
    }
    hash.update((headers.len() as u64).to_be_bytes());
    let mut canonical = [0_u8; 128];
    for (name, value) in headers {
        hash.update((name.len() as u64).to_be_bytes());
        for chunk in name.as_bytes().chunks(canonical.len()) {
            for (output, input) in canonical.iter_mut().zip(chunk) {
                *output = input.to_ascii_lowercase();
            }
            hash.update(&canonical[..chunk.len()]);
        }
        field(&mut hash, value);
    }
    format!("{:x}", hash.finalize())
}
/// Resolved values deliberately have no Debug/Serialize implementation.
pub struct ResolvedHeaders<'a>(Vec<(&'a str, std::borrow::Cow<'a, str>)>);
impl ResolvedHeaders<'_> {
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(name, value)| (*name, value.as_ref()))
    }
    pub fn redact(&self, error: anyhow::Error) -> anyhow::Error {
        if self.0.is_empty() {
            return error;
        }
        // A peer can echo escaped or truncated values, so free-text diagnostics are
        // withheld whenever custom headers are attached. Typed retry/cancellation
        // state is preserved; no credential-containing server excerpt is surfaced.
        if error.is::<super::HttpError>() {
            let mut http = error.downcast::<super::HttpError>().unwrap();
            http.excerpt =
                "request failed; response details withheld to protect custom headers".into();
            return http.into();
        }
        if error.is::<super::TransportError>() {
            let mut transport = error.downcast::<super::TransportError>().unwrap();
            transport.message =
                "model request transport failed; details withheld to protect custom headers".into();
            return transport.into();
        }
        if error.is::<super::Interrupted>() {
            return error.downcast::<super::Interrupted>().unwrap().into();
        }
        if error.is::<super::anthropic::IncompleteStream>() {
            return error
                .downcast::<super::anthropic::IncompleteStream>()
                .unwrap()
                .into();
        }
        if error.is::<super::stream::IncompleteStream>() {
            return error
                .downcast::<super::stream::IncompleteStream>()
                .unwrap()
                .into();
        }
        if error.is::<super::stream::StalledStream>() {
            return error
                .downcast::<super::stream::StalledStream>()
                .unwrap()
                .into();
        }
        if let Some(classified) = error.downcast_ref::<super::errors::Classified>() {
            // In-stream errors keep their class so retry, compaction and fallback still apply.
            return super::errors::Classified::new(
                classified.class.clone(),
                "model request failed; details withheld to protect custom headers",
            )
            .into();
        }
        if error.is::<super::responses::TerminalFailure>() {
            return error
                .downcast::<super::responses::TerminalFailure>()
                .unwrap()
                .into();
        }
        anyhow::anyhow!("model request failed; details withheld to protect custom headers")
    }
}
impl Drop for ResolvedHeaders<'_> {
    fn drop(&mut self) {
        for (_, value) in &mut self.0 {
            // Wipe secrets at the end of the dispatch, without retaining resolved values.
            if let std::borrow::Cow::Owned(value) = value {
                let mut bytes = std::mem::take(value).into_bytes();
                bytes.fill(0);
                std::hint::black_box(bytes);
            }
        }
    }
}
pub fn resolve_headers(headers: &[RequestHeader]) -> anyhow::Result<ResolvedHeaders<'_>> {
    validate_headers(headers)?;
    resolve_validated_headers(headers)
}
/// Internal immutable policies validate once when installed; env values validate per dispatch.
pub(super) fn resolve_validated_headers(
    headers: &[RequestHeader],
) -> anyhow::Result<ResolvedHeaders<'_>> {
    use std::borrow::Cow;
    let mut resolved = ResolvedHeaders(Vec::with_capacity(headers.len()));
    let mut bytes = 0;
    for row in headers {
        let value = match &row.value {
            HeaderValue::Literal(value) => Cow::Borrowed(value.as_str()),
            HeaderValue::Env(name) => Cow::Owned(std::env::var(name).map_err(|_| {
                anyhow::anyhow!("request header environment variable is missing or not Unicode")
            })?),
        };
        bytes += row.name.len() + value.len();
        resolved.0.push((row.name.as_str(), value));
        if let HeaderValue::Env(_) = &row.value {
            validate_header_value(resolved.0.last().unwrap().1.as_ref())?;
        }
        anyhow::ensure!(
            bytes <= 16_384,
            "resolved request headers exceed 16384 bytes"
        );
    }
    Ok(resolved)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub allow_insecure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ApiProtocol>,
    /// None inherits provider headers; an explicit empty list clears them.
    #[serde(
        default,
        deserialize_with = "deserialize_headers",
        skip_serializing_if = "Option::is_none"
    )]
    pub headers: Option<RequestHeaders>,
    /// Safe ChatGPT workspace identity, never a credential.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(deserialize_with = "context_override")]
    pub context_window: Option<u32>,
}
impl ModelSelection {
    /// A selection with only provider and model set; every optional field takes its default.
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            base_url: None,
            api_key_env: None,
            allow_insecure: false,
            protocol: None,
            headers: None,
            account_id: None,
            context_window: None,
        }
    }
}
fn context_override<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Number(u32),
        Text(String),
    }
    match Option::<Input>::deserialize(d)? {
        None => Ok(None),
        Some(Input::Number(n)) => Ok(Some(n)),
        Some(Input::Text(s)) if s.eq_ignore_ascii_case("auto") => Ok(None),
        _ => Err(serde::de::Error::custom(
            "context window must be auto or a token count",
        )),
    }
}

impl Default for ModelSelection {
    fn default() -> Self {
        Self {
            provider: "mlx".into(),
            model: String::new(),
            base_url: None,
            api_key_env: None,
            allow_insecure: false,
            protocol: None,
            headers: None,
            account_id: None,
            context_window: None,
        }
    }
}

/// Credential route, independent of which credentials are currently saved.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMethod {
    /// Preserve credential selection for configurations written before explicit selection existed.
    #[default]
    Auto,
    /// Use only API-key credentials; never inspect or refresh a stored sign-in.
    ApiKey,
    /// Use only the provider's sign-in credentials; never fall back to an API key.
    #[serde(rename = "oauth")]
    OAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSpec {
    pub label: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub auth: AuthMethod,
    pub enabled: bool,
    pub allow_insecure: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ApiProtocol>,
    #[serde(
        default,
        deserialize_with = "deserialize_headers",
        skip_serializing_if = "Option::is_none"
    )]
    pub headers: Option<RequestHeaders>,
    /// Further key variables for the same provider. With `api_key_env` they form a key pool: a
    /// throttled or quota-exhausted key is skipped until its cooldown ends.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub api_key_envs: Vec<String>,
}
impl Default for ProviderSpec {
    fn default() -> Self {
        Self {
            label: String::new(),
            base_url: String::new(),
            api_key_env: None,
            auth: AuthMethod::default(),
            api_key_envs: Vec::new(),
            enabled: true,
            allow_insecure: false,
            protocol: None,
            headers: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SymbolPreset {
    /// Nerd Font glyphs when the terminal advertises a Nerd Font, unicode otherwise.
    #[default]
    Auto,
    Unicode,
    Nerd,
    Ascii,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StatusPreset {
    #[default]
    Default,
    Minimal,
    Compact,
    Full,
    Nerd,
    Ascii,
    Custom,
}

/// How the pinned workers block above the composer shows running workers.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PinnedWorkers {
    /// Up to three workers, with an overflow row for the rest.
    #[default]
    Collapsed,
    /// Every running worker that fits the height budget.
    Full,
    Off,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StatusSeparator {
    Powerline,
    #[default]
    PowerlineThin,
    Slash,
    Pipe,
    Block,
    None,
    Ascii,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComposerShape {
    /// A full-width tinted card with an accent edge that follows the input state; the status line
    /// and key hints sit beneath it. No frame.
    #[default]
    Band,
    /// Rounded box; the status line is embedded in its top border and key hints in its bottom border.
    Box,
    /// Rules above and below with no side borders (cleaner when copying text).
    Line,
}

/// Everything about how the terminal UI looks and behaves that a user may tailor. All of it is
/// editable in `/settings`, stored in the same layered config as everything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// Theme name: a built-in (`dark`, `light`, `mono`, …), a file in `<dial home>/themes`, or `auto`.
    pub theme: String,
    /// Themes used by `auto`, chosen from the terminal's reported background.
    pub theme_dark: String,
    pub theme_light: String,
    pub symbols: SymbolPreset,
    /// Blue instead of green for additions in diffs.
    pub color_blind: bool,
    pub status: StatusPreset,
    pub status_separator: StatusSeparator,
    /// Segment names for `status = "custom"`: spinner, model, role, path, git, context, tokens, rate,
    /// approval, sandbox, learning, session, time.
    pub status_left: Vec<String>,
    pub status_right: Vec<String>,
    pub status_path_abbreviate: bool,
    pub status_git_counts: bool,
    /// Use the terminal background instead of a tinted status line.
    pub status_transparent: bool,
    /// Tint the composer border with a per-session color.
    pub session_accent: bool,
    pub composer_shape: ComposerShape,
    pub welcome: bool,
    pub tips: bool,
    /// Short, purposeful animation: the start-up sweep across the wordmark and the reveal when a
    /// panel opens. Off, or on a terminal without truecolor, ASCII glyphs or `NO_COLOR`, nothing moves.
    pub motion: bool,
    /// Reveal a streaming answer gradually, a share of the pending text per frame, not in network
    /// bursts. Finished, tool, and aborted output always appears whole at once.
    pub smooth_reveal: bool,
    pub show_tools: bool,
    pub session_rail: bool,
    /// Pinned block above the composer listing running workers.
    pub pinned_workers: PinnedWorkers,
    /// Second row per pinned worker: its current tool or step, with an elapsed marker after 5 s.
    pub worker_live_preview: bool,
    /// Lines of tool output shown before collapsing (Ctrl-O expands).
    pub tool_output_lines: usize,
    pub show_turn_stats: bool,
    pub show_thinking: bool,
    pub hints: bool,
    /// Ghost text completing the word under the cursor, from learned prompts and project words.
    pub word_completion: bool,
    /// Rows in the slash and file menus before they scroll.
    pub autocomplete_items: usize,
    pub history_limit: usize,
    /// Desktop/terminal notification (OSC 9, OSC 777 or OSC 99, else BEL) when a turn finishes
    /// and when a question or approval waits for you.
    pub notify: bool,
    /// Also notify when a turn stops with an error.
    pub notify_errors: bool,
    /// Mouse wheel and clicks inside full-screen panels (the inline view never captures the mouse).
    pub mouse: bool,
    /// Terminal integration where the terminal supports it: tab color and dock attention by state,
    /// OSC 9;4 progress, `SetUserVar` values, the working directory (OSC 7) and links on URLs and
    /// existing file paths in scrollback.
    pub terminal_integration: bool,
    /// Retired (links are part of `terminal_integration` now). Still accepted so existing config
    /// files load; never read or written.
    #[serde(default, skip_serializing, rename = "hyperlinks")]
    #[allow(dead_code)]
    pub retired_hyperlinks: Option<serde::de::IgnoredAny>,
}
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "auto".into(),
            theme_dark: "dial-dark".into(),
            theme_light: "dial".into(),
            symbols: SymbolPreset::Auto,
            color_blind: false,
            status: StatusPreset::Default,
            status_separator: StatusSeparator::PowerlineThin,
            status_left: vec![
                "spinner".into(),
                "model".into(),
                "path".into(),
                "git".into(),
            ],
            status_right: vec!["context".into()],
            status_path_abbreviate: true,
            status_git_counts: true,
            status_transparent: false,
            session_accent: true,
            composer_shape: ComposerShape::Band,
            welcome: true,
            tips: true,
            motion: true,
            smooth_reveal: false,
            show_tools: true,
            session_rail: false,
            pinned_workers: PinnedWorkers::Collapsed,
            worker_live_preview: true,
            tool_output_lines: 8,
            show_turn_stats: true,
            show_thinking: true,
            hints: true,
            word_completion: true,
            autocomplete_items: 12,
            history_limit: 0,
            notify: true,
            notify_errors: false,
            mouse: true,
            terminal_integration: true,
            retired_hyperlinks: None,
        }
    }
}

/// Model roles a user can assign, each with the consumers it drives. `main` is the session model and is
/// always assigned. Unassigned `judge` follows `small`, which follows `main`; `task`, `coach`, `tester`,
/// `planner`, `memory` and `turn_drafter` follow `main`. `decision` is the Laya checkpoint (never a chat
/// model) and has no chat fallback. `turn_drafter` stays last: it is listed only while
/// `experimental.turn_optimizer` is on, so hiding it never shifts another row.
pub const ROLES: &[(&str, &str)] = &[
    ("main", "Conversation and coding agent"),
    (
        "small",
        "Fast background work: compaction, commit messages, architecture synthesis and solve workers; assigned small also serves scout and custom read-only subagents",
    ),
    (
        "task",
        "Implementation tasks and worker subagents; fallback for scout, custom read-only, reviewer, test-runner and planner subagents when their preferred role is unassigned",
    ),
    (
        "coach",
        "Stronger advisor: escalate tool, automatic escalation, feedback proposals, experience teacher, advisor mode",
    ),
    (
        "tester",
        "Review and verification: reviewer and test-runner subagents and review workflows",
    ),
    ("planner", "Plan mode and planner subagents"),
    (
        "memory",
        "Memory extraction, consolidation and Sharpshooter decision memory (local, sharpshooter); required while a model-backed memory backend is enabled",
    ),
    (
        "judge",
        "Cheap typed decisions: a single choice from a fixed list, cached locally; unassigned follows small, then main",
    ),
    (
        "decision",
        "Laya decision checkpoint (local path or Hugging Face repo id) for advisory decisions; unassigned uses the installed Laya checkpoint",
    ),
    (
        "embedding",
        "Embedding model for semantic memory recall; use an OpenAI-compatible, Ollama or Gemini embedding endpoint; unassigned uses lexical recall",
    ),
    (
        TURN_DRAFTER,
        "Turn optimizer (experimental): rewrites each user message into a structured prompt with diagrams; the main model sees only the draft; unassigned follows main",
    ),
];

/// Role key of the experimental turn optimizer's drafting model.
pub const TURN_DRAFTER: &str = "turn_drafter";

/// Where a role's effective model comes from. Shown next to each role in the Alt+M Roles view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleSource {
    /// Explicitly assigned in `model_roles`.
    Assigned,
    /// The session model (`main`).
    Main,
    /// Unassigned `judge`: falls back to the small model, which itself follows the main model.
    FollowsSmall,
    /// Unassigned: falls back to the main model.
    FollowsMain,
    /// Unassigned `decision`: the installed Laya checkpoint serves decisions.
    LayaInstalled,
    /// Nothing supplies the role (required or simply unset).
    NotSet,
}

/// One Alt+M Roles row: the assigned selection, the model actually in use, and where it comes from.
#[derive(Clone, Debug)]
pub struct RoleStatus {
    pub role: &'static str,
    pub assigned: Option<ModelSelection>,
    /// Display name of the model in use; `None` when nothing supplies the role.
    pub effective: Option<String>,
    pub source: RoleSource,
}

/// Legacy `model_roles` keys and the role each one migrates to. `default` is handled separately
/// (it fills every new role that is still unassigned); `vision` has no consumer and is dropped.
pub const LEGACY_ROLE_MIGRATIONS: &[(&str, &str)] = &[
    ("smol", "small"),
    ("tiny", "small"),
    ("commit", "small"),
    ("compaction", "small"),
    ("architecture", "small"),
    ("memory", "memory"),
    ("subagent", "task"),
    ("slow", "coach"),
    ("stronger", "coach"),
    ("advisor", "coach"),
    ("feedback", "coach"),
    ("feedback-helper", "coach"),
    ("experience", "coach"),
    ("plan", "planner"),
];

impl ModelSelection {
    pub fn display_name(&self) -> String {
        let path = std::path::Path::new(&self.model);
        let name = path
            .components()
            .find_map(|part| {
                part.as_os_str()
                    .to_str()
                    .and_then(|s| s.strip_prefix("models--"))
                    .map(|s| s.replace("--", "/"))
            })
            .unwrap_or_else(|| {
                if path.is_absolute() {
                    path.file_name()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| self.model.clone())
                } else {
                    self.model.clone()
                }
            });
        match &self.account_id {
            Some(id) => format!("{}/{} [account {id}]", self.provider, name),
            None => format!("{}/{}", self.provider, name),
        }
    }
}

#[cfg(test)]
mod context_tests;

//! Provider error classification shared by retry, compaction and model fallback.
//!
//! Every provider reports context overflow, throttling and quota exhaustion in its own words.
//! [`classify`] maps an error to one [`ErrorClass`] using, in order: a class attached when the
//! error was created ([`HttpError`](super::HttpError) carries the class computed from the full
//! response body, [`Classified`] carries one for other errors), typed transport/stream errors, and
//! finally per-provider message patterns.
//!
//! Classes are attached *before* custom-header redaction replaces diagnostic text, so a redacted
//! error still classifies exactly like the original one.
use regex::{Regex, RegexSet};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

/// What kind of failure a provider request ended with, independent of the wire protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    /// The request (prompt + reserved output) does not fit the model's context window. Compaction
    /// can make room; retrying the same request cannot.
    ContextOverflow,
    /// A short-lived throttle or capacity shed (HTTP 429/529, "overloaded"). `retry_after` is the
    /// server's explicit minimum wait (`retry-after`, `retry-after-ms`, `x-ratelimit-reset[-ms]`).
    RateLimited { retry_after: Option<Duration> },
    /// An account/plan usage quota or billing cap is spent. Waiting seconds will not help; another
    /// credential or model might. `reset_at` is when the provider says the window refills.
    QuotaExhausted { reset_at: Option<SystemTime> },
    /// The credential was rejected (HTTP 401/403 without quota wording).
    Auth,
    /// A retryable transport, server or stream failure.
    Transient,
    /// The request bytes/media were rejected (HTTP 413 without token-budget wording). Token
    /// compaction cannot shrink image or byte budgets.
    Payload,
    /// Anything else: invalid request, unsupported model, user interruption, local bug.
    Fatal,
}

impl ErrorClass {
    /// Whether retrying the same request against the same credential can succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Transient)
    }

    /// Stable lowercase name for stats rows; carries no provider text.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ContextOverflow => "context_overflow",
            Self::RateLimited { .. } => "rate_limited",
            Self::QuotaExhausted { .. } => "quota_exhausted",
            Self::Auth => "auth",
            Self::Transient => "transient",
            Self::Payload => "payload",
            Self::Fatal => "fatal",
        }
    }
}

/// A non-HTTP error whose class was decided where the original diagnostic was still available.
#[derive(Debug)]
pub struct Classified {
    pub class: ErrorClass,
    message: String,
}

impl Classified {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Classified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Classified {}

/// A remote thinking loop was detected and the stream abandoned; retryable like a stall.
#[derive(Debug)]
pub struct ThinkingLoop(pub String);

impl std::fmt::Display for ThinkingLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Thinking loop detected: the model repeated near-identical reasoning ({}); treating as a stream stall and retrying",
            self.0
        )
    }
}

impl std::error::Error for ThinkingLoop {}

/// Whether compaction should run: the provider said the request exceeds the context window.
pub fn is_context_overflow(err: &anyhow::Error) -> bool {
    classify(err) == ErrorClass::ContextOverflow
}

/// Prompt size the provider measured when it rejected a request for overflow, when its message
/// states one. This is the tokenizer's real count, unlike the local byte estimate that let the
/// request through.
pub fn overflow_prompt_tokens(err: &anyhow::Error) -> Option<u64> {
    if !is_context_overflow(err) {
        return None;
    }
    err.chain().find_map(|cause| {
        let text = cause.to_string();
        OVERFLOW_COUNT
            .iter()
            .find_map(|pattern| pattern.captures(&text)?.get(1)?.as_str().parse().ok())
    })
}

static OVERFLOW_COUNT: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        one(r"(?i)prompt is too long:\s*(\d+)\s*tokens"), // Anthropic
        one(r"(?i)input token count\s*\(?(\d+)\)?"),      // Gemini
        one(r"(?i)\((\d+)\s+in the messages"),            // OpenAI legacy
        one(r"(?i)resulted in\s+(\d+)\s+tokens"),         // OpenAI / vLLM
        one(r"(?i)\bprompt\s*\(\s*(\d+)\s+tokens\s*\)"),  // llama.cpp
    ]
});

/// The server rejected the model's tool-call output (a model fault, not an infrastructure one).
pub(crate) fn is_model_output_rejection(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<super::HttpError>()
            .is_some_and(|http| http.status == 500 && LLAMA_TOOL_PARSE.is_match(&http.to_string()))
    })
}

/// Classify any error returned by a provider request.
pub fn classify(err: &anyhow::Error) -> ErrorClass {
    for cause in err.chain() {
        if let Some(http) = cause.downcast_ref::<super::HttpError>() {
            return http.class.clone();
        }
        if let Some(classified) = cause.downcast_ref::<Classified>() {
            return classified.class.clone();
        }
        if cause.downcast_ref::<super::Interrupted>().is_some() {
            return ErrorClass::Fatal;
        }
        if cause.downcast_ref::<ThinkingLoop>().is_some()
            || cause
                .downcast_ref::<super::stream::IncompleteStream>()
                .is_some()
            || cause
                .downcast_ref::<super::stream::StalledStream>()
                .is_some()
            || cause
                .downcast_ref::<super::anthropic::IncompleteStream>()
                .is_some()
        {
            return ErrorClass::Transient;
        }
        if let Some(transport) = cause.downcast_ref::<super::TransportError>() {
            return if transport.connection {
                ErrorClass::Transient
            } else {
                ErrorClass::Fatal
            };
        }
        if let Some(failure) = cause.downcast_ref::<super::responses::TerminalFailure>() {
            return failure.class();
        }
    }
    classify_text(None, &format!("{err:#}"), None, None)
}

/// Classify an HTTP failure from its status and the full (untruncated, unredacted) body text.
pub(super) fn classify_http(
    status: u16,
    text: &str,
    retry_after: Option<Duration>,
    reset_at: Option<SystemTime>,
) -> ErrorClass {
    classify_text(Some(status), text, retry_after, reset_at)
}

/// Classify an in-stream `error` event from its wire `type`/`code` values. Quota, payload and
/// context-overflow wording wins; otherwise overload and rate-limit kinds map to their classes.
pub(super) fn classify_stream_error(kinds: &[&str], message: &str) -> ErrorClass {
    let text_class = classify_text(None, message, None, None);
    if matches!(
        text_class,
        ErrorClass::ContextOverflow | ErrorClass::Payload | ErrorClass::QuotaExhausted { .. }
    ) {
        return text_class;
    }
    for kind in kinds {
        match *kind {
            "rate_limit_exceeded" | "rate_limit_error" | "rate_limit" | "too_many_requests" => {
                return ErrorClass::RateLimited { retry_after: None };
            }
            "server_error"
            | "api_error"
            | "internal_error"
            | "overloaded"
            | "overloaded_error"
            | "service_unavailable"
            | "timeout" => return ErrorClass::Transient,
            _ => {}
        }
    }
    text_class
}

fn classify_text(
    status: Option<u16>,
    text: &str,
    retry_after: Option<Duration>,
    reset_at: Option<SystemTime>,
) -> ErrorClass {
    // Throttle statuses are never context overflows; "too many tokens per minute" is a TPM throttle.
    let throttled = matches!(status, Some(429 | 529));
    let token_overflow = !throttled && OVERFLOW.is_match(text);
    if token_overflow {
        return ErrorClass::ContextOverflow;
    }
    if status == Some(413) || PAYLOAD.is_match(text) {
        return ErrorClass::Payload;
    }
    if !throttled && (GENERIC_LIMIT.is_match(text) || OVERFLOW_NO_BODY.is_match(text)) {
        return ErrorClass::ContextOverflow;
    }
    let throttle = rolling_throttle(text, retry_after);
    let quota = !throttle && is_usage_limit_text(text);
    match status {
        Some(401) => {
            return if quota {
                ErrorClass::QuotaExhausted { reset_at }
            } else {
                ErrorClass::Auth
            };
        }
        Some(403) => {
            return if quota || ACCOUNT_SCOPED_CAP.is_match(text) {
                ErrorClass::QuotaExhausted { reset_at }
            } else {
                ErrorClass::Auth
            };
        }
        Some(402) => return ErrorClass::QuotaExhausted { reset_at },
        Some(429) => {
            let throttle = rolling_throttle(text, retry_after);
            return if quota && !CONCURRENT_LIMIT.is_match(text) && !throttle {
                ErrorClass::QuotaExhausted { reset_at }
            } else {
                ErrorClass::RateLimited { retry_after }
            };
        }
        Some(529) => return ErrorClass::RateLimited { retry_after },
        Some(500) if LLAMA_TOOL_PARSE.is_match(text) => return ErrorClass::Fatal,
        Some(400) if GENERATION_NAN.is_match(text) => return ErrorClass::Transient,
        Some(408 | 409 | 425 | 500 | 502 | 503 | 504) => return ErrorClass::Transient,
        Some(_) => return ErrorClass::Fatal,
        None => {}
    }
    // Text-only errors: stream error events and wrapped diagnostics without an HTTP status.
    if throttle {
        return ErrorClass::RateLimited { retry_after };
    }
    if quota {
        return ErrorClass::QuotaExhausted { reset_at };
    }
    if THROTTLE.is_match(text) {
        return ErrorClass::RateLimited { retry_after };
    }
    if TRANSIENT_TEXT.is_match(text) && !LLAMA_TOOL_PARSE.is_match(text) {
        return ErrorClass::Transient;
    }
    ErrorClass::Fatal
}

/// Account/plan quota exhaustion, as opposed to a rolling per-minute throttle.
fn is_usage_limit_text(text: &str) -> bool {
    if DASHSCOPE_TOKEN_LIMIT_DOC.is_match(text) && DASHSCOPE_TOKEN_LIMIT_MESSAGE.is_match(text) {
        return false;
    }
    let quota_reason = QUOTA_REASON.is_match(text)
        || SUBSCRIPTION_CAP.is_match(text) && !PER_INTERVAL.is_match(text);
    if TPM_RPM_THROTTLE.is_match(text) && !quota_reason {
        return false;
    }
    USAGE_LIMIT.is_match(text) || quota_reason
}

/// Google `RESOURCE_EXHAUSTED` is a rolling throttle, not account quota, when the server gives a
/// retry delay or the text names a per-minute window, unless the text names a day or month window.
pub(super) fn rolling_throttle(text: &str, retry_after: Option<Duration>) -> bool {
    RESOURCE_EXHAUSTED.is_match(text)
        && !LONG_WINDOW.is_match(text)
        && (retry_after.is_some() || PER_MINUTE_WINDOW.is_match(text))
}
static RESOURCE_EXHAUSTED: LazyLock<Regex> = LazyLock::new(|| one(r"(?i)resource.?exhausted"));
static LONG_WINDOW: LazyLock<Regex> =
    LazyLock::new(|| one(r"(?i)per[-_ ]?day|daily|per[-_ ]?month|monthly"));
static PER_MINUTE_WINDOW: LazyLock<Regex> =
    LazyLock::new(|| one(r"(?i)per[-_ ]?minute|per[-_ ]min\b|\b(?:rpm|tpm)\b"));

fn set(patterns: &[&str]) -> RegexSet {
    RegexSet::new(patterns).expect("static provider error patterns")
}

fn one(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static provider error pattern")
}

/// Token-budget overflow evidence, per provider.
static OVERFLOW: LazyLock<RegexSet> = LazyLock::new(|| {
    set(&[
        r"(?i)prompt is too long",                     // Anthropic
        r"(?i)input is too long for requested model",  // Amazon Bedrock
        r"(?i)exceeds the context window",             // OpenAI Completions & Responses
        r"(?i)input token count.*exceeds the maximum", // Google Gemini
        r"(?i)maximum prompt length is \d+",           // xAI Grok
        r"(?i)reduce the length of the messages",      // Groq
        r"(?i)maximum context length is \d+ tokens",   // OpenRouter, vLLM, OpenAI legacy
        r"(?i)exceeds the available context size",     // llama.cpp server
        r"(?i)\bprompt\s*\(\s*\d+\s+tokens\s*\)\s*\+\s*max\s+tokens\s*\(\s*\d+\s*\)\s+exceeds\s+the\s+context\s*\(\s*\d+\s*\)",
        r"(?i)\bprompt\s*\(\s*\d+\s+tokens\s*\)\s+leaves\s+no\s+room\s+to\s+answer\s+in\s+the\s+context\s*\(\s*\d+\s*\)",
        r"(?i)requested tokens?.*exceed.*context (window|length|size)", // llama.cpp / local servers
        r"(?i)context (window|length|size).*(exceeded|overflow|too small)",
        r"(?i)(prompt|input).*(too long|too large).*(context|n_ctx)",
        r"(?i)requested tokens?.*(exceeds?|greater than).*(n_ctx|context)",
        r"(?i)greater than the context length", // LM Studio
        r"(?i)context window exceeds limit",    // MiniMax
        r"(?i)exceeded model token limit",      // Kimi
        r"(?i)context[_ ]length[_ ]exceeded",   // OpenAI code, generic
        r"(?i)too many tokens",
        r"(?i)token limit exceeded",
        r"(?i)request_too_large[^\n]*\btokens?\b",
        r"(?i)\btokens?\b[^\n]*request_too_large",
        r"(?i)model_context_window_exceeded", // z.ai finish reason
        r"(?i)prompt filled the context window", // Ollama empty length completion
        r"(?i)exceeds the limit of \d+ tokens?\b",
        r"(?i)chat history exceeds the \d+-message limit",
        r"(?i)input length .*exceeds.*(maximum|max).*(context|model)", // vLLM
        r"(?i)this model's maximum context length", // OpenAI / Mistral / DeepSeek
        r"(?i)prompt too long; exceeded (max )?context length", // Mistral
        r"(?i)input validation error.*(inputs|tokens).*must be <=", // TGI
    ])
});

/// Numeric limit wording that media budgets share; never vetoes a payload rejection.
static GENERIC_LIMIT: LazyLock<Regex> = LazyLock::new(|| one(r"(?i)exceeds the limit of \d+"));
static OVERFLOW_NO_BODY: LazyLock<Regex> =
    LazyLock::new(|| one(r"(?i)\b400\s*(status code)?\s*\(no body\)"));

/// HTTP 413 byte/media rejections without token wording.
static PAYLOAD: LazyLock<RegexSet> = LazyLock::new(|| {
    set(&[
        r"(?i)\b413\s*(?:status code\s*)?\(no body\)",
        r"(?i)\b413\b[^.\n]{0,120}\b(?:request|payload|entity|body)\b[^.\n]{0,60}\b(?:exceed|too large|limit)",
        r"(?i)request_too_large",
        r"(?i)(?:payload|entity) too large",
        r"(?i)request exceeds the maximum (?:size|number of bytes)",
    ])
});

/// Usage-limit wording plus credit/plan/spend phrasings.
static USAGE_LIMIT: LazyLock<RegexSet> = LazyLock::new(|| {
    set(&[
        r"(?i)usage.?limit|usage_limit_reached|usage_not_included|limit_reached|quota.?(?:exceeded|reached|insufficient)|resource.?exhausted|exhausted your capacity|quota will reset|insufficient.?(?:balance|quota)|balance.?exhausted|run out of credits|out of credits|out of (?:extra )?usage|spending[- _]?limit|personal-team-blocked|clinepass limit|free limit reached on model|access_terminated_error|billing_hard_limit_reached|subscription_sharing_usage",
        r"额度不足|额度耗尽",
        r"(?i)\busage credits are required\b|\bcredits_required\b",
        r"(?i)\busage_pricing_required\b|\bprepaid balance\b[^\n]{0,40}\b(?:used up|exhausted|depleted)\b",
        r"(?i)\b(?:exceed\w*|insufficient|not enough)\b[^\n]{0,40}\bcredits?\b|\bcredits?\b[^\n]{0,40}\b(?:exhausted|depleted)\b",
        r"(?:使用|用量).{0,30}?上限|(?:额度|配额)已?(?:用|耗)(?:完|尽)|限额.{0,30}重置|余额不足",
        r"(?i)spend(?:ing)?[\s_-]?(?:limit|cap)\b",
        r"(?i)\baccount(?:'s)?\b[^\n]{0,80}\brate.?limit\b|\brate.?limit\b[^\n]{0,80}\baccount\b",
        r"(?i)\bfree[-_ ]models[-_ ]per[-_ ]day\b",
    ])
});

/// Explicit quota-exhausted reasons.
static QUOTA_REASON: LazyLock<RegexSet> = LazyLock::new(|| {
    set(&[
        r"(?i)quota will reset|exhausted your capacity",
        r"(?i)\baccount(?:'s)?\b[^\n]{0,80}\brate.?limit\b|\brate.?limit\b[^\n]{0,80}\baccount\b",
        r"(?i)spend(?:ing)?[\s_-]?(?:limit|cap)\b",
        r"(?i)\bfree[-_ ]models[-_ ]per[-_ ]day\b",
        r"(?i)insufficient.?balance|run out of credits|out of credits|access_terminated_error",
    ])
});

static SUBSCRIPTION_CAP: LazyLock<Regex> = LazyLock::new(|| {
    one(
        r"(?i)\b(?:subscription|plan|membership)\b[^\n]{0,80}\b(?:rate.?limits?|quota|cap)\b|\b(?:rate.?limits?|quota|cap)\b[^\n]{0,80}\b(?:subscription|plan|membership)\b",
    )
});
static PER_INTERVAL: LazyLock<Regex> = LazyLock::new(|| one(r"(?i)\bper\s+(?:second|minute)\b"));
static TPM_RPM_THROTTLE: LazyLock<Regex> = LazyLock::new(|| {
    one(
        r"(?i)\b(?:tpm|rpm)\b[^\n]{0,40}\b(?:exhaust\w*|exceed\w*|limit\w*|throttl\w*|reach\w*)\b|\b(?:exhaust\w*|exceed\w*|limit\w*|throttl\w*|reach\w*)\b[^\n]{0,40}\b(?:tpm|rpm)\b|\bRateLimitExceeded\.(?:Endpoint)?(?:TPM|RPM)\w*",
    )
});
static DASHSCOPE_TOKEN_LIMIT_DOC: LazyLock<Regex> =
    LazyLock::new(|| one(r"(?i)error-code[^()\s]*#token-limit"));
static DASHSCOPE_TOKEN_LIMIT_MESSAGE: LazyLock<Regex> = LazyLock::new(|| {
    one(r"(?i)\byou exceeded your current quota, please check your plan and billing details\b")
});
static ACCOUNT_SCOPED_CAP: LazyLock<Regex> = LazyLock::new(|| {
    one(
        r"(?i)\b(?:overall|account|organization|team|workspace)\b[^\n]{0,40}\b(?:message |request )?rate.?limit\b|\byour\b[^\n]{0,30}\b(?:limit )?will reset\b",
    )
});
static CONCURRENT_LIMIT: LazyLock<Regex> = LazyLock::new(|| {
    one(
        r"(?i)\btoo many\s+concurren\w*\s+(?:requests?|invocations?)\b|\bconcurren\w*\b[^\n]{0,60}\b(?:limit|quota|exceed\w*|reach\w*)\b",
    )
});
static THROTTLE: LazyLock<Regex> = LazyLock::new(|| {
    one(
        r"(?i)rate.?limit|too many requests|overloaded|\b(?:no[_ -]?capacity|(?:high|peak)[ _-]?demand|(?:at|over|insufficient)[ _-]?capacity|capacity[ _-]?(?:exceeded|exhausted))\b",
    )
});
static TRANSIENT_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    one(
        r"(?i)service.?unavailable|internal.?server.?error|\binternal error\b|bad gateway|gateway time-?out|upstream.?connect|upstream.?request.?failed|reset before headers|stream stall|stream[_ -]?read[_ -]?error|connection (?:reset|refused|closed)|other side closed|socket hang up|timed? out\b",
    )
});
static LLAMA_TOOL_PARSE: LazyLock<Regex> = LazyLock::new(|| {
    one(r"(?i)failed to parse tool call arguments as json|\[json\.exception\.parse_error\.101\]")
});
static GENERATION_NAN: LazyLock<Regex> =
    LazyLock::new(|| one(r"(?is)floating[ _-]?point nan\b.*\bdetected in generation"));

#[cfg(test)]
mod tests;

//! Bundled provider registry and model list-price/capability catalog.
//!
//! `providers.json` and `models.json` are maintained in this repository (attribution in
//! `THIRD_PARTY_NOTICES`). Rows are
//! published list prices and documented limits, not observations: callers label prices
//! derived here as estimates and prefer live provider metadata or provider-reported costs.
use super::{
    CacheWriteSplit, ModelKind, ModelMetadata, ONE_HOUR_WRITE_INPUT_MULTIPLIER, TokenPricing,
};
use crate::llm::{
    Reply,
    model_config::{ApiProtocol, ModelSelection},
    profiles::ReasoningEffort,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::LazyLock,
};

const PROVIDERS_JSON: &str = include_str!("providers.json");
const MODELS_JSON: &str = include_str!("models.json");

/// A hosted API-key provider Dial can reach without hand configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinProvider {
    pub id: String,
    pub label: String,
    pub base_url: String,
    /// Credential variables bound to this provider's origin: Dial refuses to send them anywhere else.
    pub env: Vec<String>,
    /// Generic credential variables (e.g. `HF_TOKEN`) read for this provider but not origin-bound.
    pub shared_env: Vec<String>,
    pub protocol: ApiProtocol,
    /// Listed (and probed) by discovery even when no credential is configured.
    #[serde(rename = "default")]
    pub listed: bool,
}
impl BuiltinProvider {
    pub fn env_keys(&self) -> impl Iterator<Item = &str> {
        self.env.iter().chain(&self.shared_env).map(String::as_str)
    }
}

#[derive(Deserialize)]
struct ProviderFile {
    providers: Vec<BuiltinProvider>,
}

static PROVIDERS: LazyLock<Vec<BuiltinProvider>> = LazyLock::new(|| {
    serde_json::from_str::<ProviderFile>(PROVIDERS_JSON)
        .expect("bundled providers.json is generated valid JSON")
        .providers
});

pub fn builtin_providers() -> &'static [BuiltinProvider] {
    &PROVIDERS
}

pub fn builtin_provider(id: &str) -> Option<&'static BuiltinProvider> {
    builtin_providers()
        .iter()
        .find(|provider| provider.id == id)
}

/// Base URL, primary credential variable and wire protocol of a built-in provider id.
pub fn provider_endpoint(id: &str) -> Option<(&'static str, Option<&'static str>, ApiProtocol)> {
    builtin_provider(id).map(|provider| {
        (
            provider.base_url.as_str(),
            provider.env_keys().next(),
            provider.protocol,
        )
    })
}

/// How a model exposes reasoning control, as recorded by the upstream catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingMode {
    /// OpenAI-style `reasoning_effort` levels.
    Effort,
    /// A token budget.
    Budget,
    /// Anthropic adaptive thinking with `output_config.effort`.
    AnthropicAdaptive,
    /// Anthropic budget thinking that also accepts an effort.
    AnthropicBudgetEffort,
    /// Gemini `thinkingLevel`.
    GoogleLevel,
}

/// USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rates {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}
impl Rates {
    fn new([input, output, cache_read, cache_write]: [f64; 4]) -> Option<Self> {
        [input, output, cache_read, cache_write]
            .iter()
            .all(|rate| rate.is_finite() && *rate >= 0.0)
            .then_some(Self {
                input,
                output,
                cache_read,
                cache_write,
            })
    }
}

#[derive(Debug)]
struct LongContext {
    threshold: u64,
    inclusive: bool,
    rates: Rates,
}

/// Recurring UTC peak windows; outside them every rate is multiplied by `off_peak`.
#[derive(Debug)]
struct Schedule {
    off_peak: f64,
    /// (weekdays with 0 = Sunday, start minute, end minute) in UTC.
    peak: Vec<(Vec<u8>, u32, u32)>,
    /// Rates that replace the base card from a Unix-millisecond instant onward.
    effective: Vec<(u64, Rates)>,
}
impl Schedule {
    fn is_peak(&self, now_ms: u64) -> bool {
        // The Unix epoch was a Thursday; arithmetic keeps this UTC-only.
        let day = now_ms / 86_400_000;
        let weekday = ((day + 4) % 7) as u8;
        let minute = ((now_ms - day * 86_400_000) / 60_000) as u32;
        self.peak.iter().any(|(days, start, end)| {
            let in_window = if start <= end {
                minute >= *start && minute < *end
            } else {
                // A window whose end precedes its start wraps past UTC midnight.
                minute >= *start || minute < *end
            };
            in_window && days.contains(&weekday)
        })
    }
}

/// A published list-price card: base rates, optional long-context tier and off-peak schedule.
#[derive(Debug)]
pub struct ListPrice {
    base: Rates,
    long_context: Option<LongContext>,
    schedule: Option<Schedule>,
}
impl ListPrice {
    /// Price a reply at `now_ms` (Unix milliseconds).
    /// `prompt_tokens` includes cached and cache-written tokens, as everywhere in Dial. Missing
    /// usage is unknown, never free.
    pub fn reply_cost(&self, reply: &Reply, now_ms: u64) -> Option<f64> {
        let prompt = reply.prompt_tokens?;
        let completion = reply.completion_tokens?;
        let read = reply.cached_prompt_tokens.unwrap_or(0);
        let write = reply.cache_creation_tokens.unwrap_or(0);
        let uncached = prompt.checked_sub(read.checked_add(write)?)?;
        let mut rates = self.base;
        let mut since = None;
        for (from, candidate) in self.schedule.iter().flat_map(|s| &s.effective) {
            if *from <= now_ms && since.is_none_or(|since| *from > since) {
                rates = *candidate;
                since = Some(*from);
            }
        }
        // A replacement card from `effective` carries no long-context tier of its own.
        if since.is_none()
            && let Some(long) = &self.long_context
            && (prompt > long.threshold || (long.inclusive && prompt == long.threshold))
        {
            rates = long.rates;
        }
        let multiplier = match &self.schedule {
            Some(schedule) if !schedule.is_peak(now_ms) => schedule.off_peak,
            _ => 1.0,
        };
        let split = CacheWriteSplit::of(reply)?;
        // The 1h write rate is the input rate times the 1h multiplier.
        let cost = (uncached as f64 * rates.input
            + completion as f64 * rates.output
            + read as f64 * rates.cache_read
            + (split.five_minute + split.unbucketed) as f64 * rates.cache_write
            + split.one_hour as f64 * rates.input * ONE_HOUR_WRITE_INPUT_MULTIPLIER)
            / 1_000_000.0
            * multiplier;
        (cost.is_finite() && cost >= 0.0).then_some(cost)
    }

    /// The base card as per-token prices, for callers that only need an indicative rate.
    pub fn token_pricing(&self) -> TokenPricing {
        TokenPricing {
            prompt: self.base.input / 1_000_000.0,
            completion: self.base.output / 1_000_000.0,
            cache_read: Some(self.base.cache_read / 1_000_000.0),
            cache_write: Some(self.base.cache_write / 1_000_000.0),
        }
    }
}

/// One bundled chat model row.
#[derive(Clone, Debug)]
pub struct BundledModel {
    pub id: String,
    /// Catalog provider id (Dial id where Dial has one, e.g. `gemini`, `chatgpt`).
    pub provider: &'static str,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub reasoning: bool,
    pub vision: bool,
    pub thinking: Option<ThinkingMode>,
    /// Supported reasoning levels, weakest first, exactly as documented for this model.
    pub thinking_efforts: Vec<ReasoningEffort>,
    /// Provider wire value per declared effort (e.g. `minimal` sent as `instant`).
    pub effort_wire: BTreeMap<ReasoningEffort, String>,
    pub default_effort: Option<ReasoningEffort>,
    /// Base list price per token; `None` when the upstream card is unknown or variable.
    pub pricing: Option<TokenPricing>,
    pub list_price: Option<&'static ListPrice>,
}
impl BundledModel {
    /// Capability metadata for a discovery entry; live provider fields should take precedence.
    pub fn metadata(&self) -> ModelMetadata {
        ModelMetadata {
            vision: Some(self.vision),
            reasoning: Some(self.reasoning),
            max_output_tokens: self.max_output_tokens.filter(|n| *n > 0),
            pricing: self.pricing,
            ..Default::default()
        }
    }
}

#[derive(Deserialize)]
struct Row {
    id: String,
    c: Option<u32>,
    o: Option<u32>,
    #[serde(default)]
    p: Option<[f64; 4]>,
    #[serde(default)]
    lc: Option<(u64, bool, f64, f64, f64, f64)>,
    #[serde(default)]
    tb: Option<ScheduleRow>,
    #[serde(default)]
    r: u8,
    #[serde(default)]
    v: u8,
    #[serde(default)]
    t: Option<ThinkingMode>,
    #[serde(default)]
    e: Vec<ReasoningEffort>,
    #[serde(default)]
    d: Option<ReasoningEffort>,
    /// Provider wire value per declared effort when it differs from the Dial name.
    #[serde(default)]
    m: BTreeMap<ReasoningEffort, String>,
}
#[derive(Deserialize)]
struct ScheduleRow {
    m: f64,
    w: Vec<(Vec<u8>, u32, u32)>,
    #[serde(default)]
    e: Vec<(u64, f64, f64, f64, f64)>,
}
#[derive(Deserialize)]
struct ModelFile {
    models: HashMap<String, Vec<Row>>,
}

struct Entry {
    id: String,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    reasoning: bool,
    vision: bool,
    thinking: Option<ThinkingMode>,
    efforts: Vec<ReasoningEffort>,
    default_effort: Option<ReasoningEffort>,
    effort_wire: BTreeMap<ReasoningEffort, String>,
    price: Option<ListPrice>,
}
struct ProviderModels {
    entries: Vec<Entry>,
    /// Lowercased model id -> entry index.
    index: HashMap<String, usize>,
}

fn list_price(row: &Row) -> Option<ListPrice> {
    let base = Rates::new(row.p?)?;
    let long_context = match row.lc {
        Some((threshold, inclusive, input, output, read, write)) => Some(LongContext {
            threshold,
            inclusive,
            rates: Rates::new([input, output, read, write])?,
        }),
        None => None,
    };
    let schedule = match &row.tb {
        Some(tb) => {
            if !(tb.m.is_finite() && tb.m >= 0.0) {
                return None;
            }
            let mut effective = Vec::with_capacity(tb.e.len());
            for (from, input, output, read, write) in &tb.e {
                effective.push((*from, Rates::new([*input, *output, *read, *write])?));
            }
            Some(Schedule {
                off_peak: tb.m,
                peak: tb.w.clone(),
                effective,
            })
        }
        None => None,
    };
    Some(ListPrice {
        base,
        long_context,
        schedule,
    })
}

/// Wire-value overrides may name only efforts the row declares, and each must be a non-empty
/// token without surrounding whitespace.
fn validate_effort_wire(row: &Row) -> Result<(), String> {
    for (effort, wire) in &row.m {
        if !row.e.contains(effort) {
            return Err(format!(
                "{}: wire value for effort {} that is not declared",
                row.id,
                effort.as_str()
            ));
        }
        if wire.is_empty() || wire.trim() != wire {
            return Err(format!(
                "{}: invalid wire value for {}",
                row.id,
                effort.as_str()
            ));
        }
    }
    Ok(())
}

static MODELS: LazyLock<HashMap<String, ProviderModels>> = LazyLock::new(|| {
    let file: ModelFile =
        serde_json::from_str(MODELS_JSON).expect("bundled models.json is generated valid JSON");
    file.models
        .into_iter()
        .map(|(provider, rows)| {
            let mut index = HashMap::with_capacity(rows.len());
            let entries: Vec<Entry> = rows
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    index.entry(row.id.to_ascii_lowercase()).or_insert(i);
                    validate_effort_wire(&row)
                        .expect("bundled models.json effort wire map is valid");
                    Entry {
                        price: list_price(&row),
                        context_window: row.c.filter(|n| *n > 0),
                        max_output_tokens: row.o.filter(|n| *n > 0),
                        reasoning: row.r != 0,
                        vision: row.v != 0,
                        thinking: row.t,
                        efforts: row.e,
                        default_effort: row.d,
                        effort_wire: row.m,
                        id: row.id,
                    }
                })
                .collect();
            (provider, ProviderModels { entries, index })
        })
        .collect()
});

fn catalog() -> &'static HashMap<String, ProviderModels> {
    &MODELS
}

/// Catalog ids for providers Dial names differently.
fn catalog_id(provider: &str) -> &str {
    match provider {
        "google" => "gemini",
        "openai-codex" => "chatgpt",
        other => other,
    }
}

fn to_bundled(provider: &'static str, entry: &'static Entry) -> BundledModel {
    BundledModel {
        id: entry.id.clone(),
        provider,
        context_window: entry.context_window,
        max_output_tokens: entry.max_output_tokens,
        reasoning: entry.reasoning,
        vision: entry.vision,
        thinking: entry.thinking,
        thinking_efforts: entry.efforts.clone(),
        effort_wire: entry.effort_wire.clone(),
        default_effort: entry.default_effort,
        pricing: entry.price.as_ref().map(ListPrice::token_pricing),
        list_price: entry.price.as_ref(),
    }
}

/// Fireworks publishes `accounts/fireworks/models/kimi-k2p6`; the catalog keys `kimi-k2.6`.
fn fireworks_public_id(model: &str) -> Option<String> {
    let stripped = model
        .strip_prefix("accounts/fireworks/models/")
        .or_else(|| model.strip_prefix("accounts/fireworks/routers/"))?;
    let bytes = stripped.as_bytes();
    Some(
        stripped
            .char_indices()
            .map(|(i, c)| {
                let digit =
                    |j: Option<usize>| j.and_then(|j| bytes.get(j)).is_some_and(u8::is_ascii_digit);
                if c == 'p' && digit(i.checked_sub(1)) && digit(Some(i + 1)) {
                    '.'
                } else {
                    c
                }
            })
            .collect(),
    )
}

fn find(provider: &str, model: &str) -> Option<BundledModel> {
    let (key, models) = catalog().get_key_value(catalog_id(provider))?;
    let lower = model.trim().to_ascii_lowercase();
    let mut candidates = vec![lower.clone()];
    if let Some(stripped) = lower.strip_prefix("models/") {
        candidates.push(stripped.to_owned());
    }
    if let Some(public) = fireworks_public_id(&lower) {
        candidates.push(public);
    }
    if let Some((base, suffix)) = lower.rsplit_once('-')
        && suffix.len() == 8
        && suffix.starts_with("20")
        && suffix.bytes().all(|byte| byte.is_ascii_digit())
    {
        candidates.push(base.to_owned());
    }
    candidates
        .iter()
        .find_map(|id| models.index.get(id))
        .map(|&i| to_bundled(key.as_str(), &models.entries[i]))
}

/// The bundled row for `model` as served by `provider` (a Dial or catalog provider id).
/// ChatGPT subscription models fall back to the OpenAI API card for API-equivalent pricing.
pub fn bundled_model(provider: &str, model: &str) -> Option<BundledModel> {
    find(provider, model).or_else(|| match catalog_id(provider) {
        "chatgpt" => find("openai", model),
        _ => None,
    })
}

/// Every bundled chat model of a provider (Dial or catalog id), in catalog order.
pub fn bundled_models(provider: &str) -> Vec<BundledModel> {
    catalog()
        .get_key_value(catalog_id(provider))
        .map(|(key, models)| {
            models
                .entries
                .iter()
                .map(|entry| to_bundled(key.as_str(), entry))
                .collect()
        })
        .unwrap_or_default()
}

/// Providers searched first when a model id is not qualified by a provider: the vendors that
/// publish the model themselves, so a reseller's card does not stand in for the original.
const FIRST_PARTY: &[&str] = &[
    "anthropic",
    "openai",
    "gemini",
    "deepseek",
    "mistral",
    "xai",
    "moonshot",
    "zai",
    "minimax",
    "groq",
    "cerebras",
    "together",
    "fireworks",
    "openrouter",
];

fn unqualified(model: &str) -> Option<BundledModel> {
    let mut rest: Vec<&str> = catalog()
        .keys()
        .map(String::as_str)
        .filter(|provider| !FIRST_PARTY.contains(provider))
        .collect();
    rest.sort_unstable();
    FIRST_PARTY
        .iter()
        .copied()
        .chain(rest)
        .find_map(|provider| find(provider, model))
}

/// Capabilities and list price for a bare model id or a `provider/model` selector.
pub fn model_capabilities(model: &str) -> Option<BundledModel> {
    let model = model.trim();
    if let Some((provider, rest)) = model.split_once('/')
        && catalog().contains_key(catalog_id(provider))
        && let Some(found) = bundled_model(provider, rest)
    {
        return Some(found);
    }
    unqualified(model).or_else(|| {
        // `vendor/model` ids (OpenRouter style) also match the vendor's own bare id.
        let (_, bare) = model.rsplit_once('/')?;
        unqualified(bare)
    })
}

/// Documented context window for a bare model id or a `provider/model` selector.
pub fn known_context_window(model: &str) -> Option<u32> {
    model_capabilities(model)?.context_window
}

fn origin(url: &str) -> Option<(String, String, u16)> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    Some(super::origin(&uri))
}

/// The built-in providers serving `base_url`: exact base matches first, then same-origin ones.
pub(super) fn providers_for_url(base_url: &str) -> Vec<&'static BuiltinProvider> {
    let base = base_url.trim_end_matches('/');
    let Some(target) = origin(base) else {
        return Vec::new();
    };
    let mut exact = Vec::new();
    let mut same_origin = Vec::new();
    for provider in builtin_providers() {
        if provider.base_url.trim_end_matches('/') == base {
            exact.push(provider);
        } else if origin(&provider.base_url).is_some_and(|o| o == target) {
            same_origin.push(provider);
        }
    }
    exact.extend(same_origin);
    exact
}

/// The catalog provider that prices `selection`, matched by endpoint origin so a custom proxy
/// never inherits a first-party card. Local endpoints have no list price.
pub(crate) fn selection_catalog_provider(selection: &ModelSelection) -> Option<&'static str> {
    if selection.provider == crate::llm::chatgpt::PROVIDER {
        return Some("chatgpt");
    }
    let Some(base) = selection.base_url.as_deref() else {
        if selection.provider == "mlx" {
            return None;
        }
        return builtin_provider(&selection.provider)
            .map(|provider| provider.id.as_str())
            .or_else(|| {
                catalog()
                    .get_key_value(catalog_id(&selection.provider))
                    .map(|(k, _)| k.as_str())
            });
    };
    if crate::llm::host_of(base).is_none_or(|host| crate::llm::is_local_host(&host)) {
        return None;
    }
    let candidates = providers_for_url(base);
    candidates
        .iter()
        .find(|provider| provider.id == selection.provider)
        .or_else(|| candidates.first())
        .map(|provider| provider.id.as_str())
}

/// The bundled row for a configured selection, if its endpoint is a known provider.
pub(crate) fn selection_model(selection: &ModelSelection) -> Option<BundledModel> {
    bundled_model(selection_catalog_provider(selection)?, &selection.model)
}

/// Fill capability gaps the live catalog left open and attach the bundled list price.
pub(super) fn complete_metadata(
    selection: &ModelSelection,
    metadata: ModelMetadata,
) -> ModelMetadata {
    let mut metadata = complete_capabilities(selection, metadata);
    if let Some(model) = selection_model(selection) {
        metadata.pricing = metadata
            .pricing
            .or_else(|| model.list_price.map(ListPrice::token_pricing));
    }
    metadata
}

/// Fill purpose, vision, reasoning and output-limit gaps from the exact bundled record. Values the
/// live catalog reported win; pricing is left to [`complete_metadata`].
pub(super) fn complete_capabilities(
    selection: &ModelSelection,
    mut metadata: ModelMetadata,
) -> ModelMetadata {
    metadata.kind = known_kind(selection, metadata.kind);
    if let Some(model) = selection_model(selection) {
        metadata.vision = metadata.vision.or(Some(model.vision));
        metadata.reasoning = metadata.reasoning.or(Some(model.reasoning));
        metadata.max_output_tokens = metadata.max_output_tokens.or(model.max_output_tokens);
    }
    metadata
}

/// Documented purpose of exact provider model ids whose model lists carry no purpose field. Only
/// rows the provider documents by exact id belong here; family or name patterns never do.
const EXACT_KINDS: &[(&str, &str, ModelKind)] = &[
    ("openai", "text-embedding-3-small", ModelKind::Embedding),
    ("openai", "text-embedding-3-large", ModelKind::Embedding),
    ("openai", "text-embedding-ada-002", ModelKind::Embedding),
];

/// Purpose a live row reported, else the documented purpose of the exact provider id. An unknown
/// purpose stays `Unknown`: custom and unlisted models are not assumed to be embedders or chat.
pub(super) fn known_kind(selection: &ModelSelection, reported: ModelKind) -> ModelKind {
    if reported != ModelKind::Unknown {
        return reported;
    }
    let Some(provider) = selection_catalog_provider(selection) else {
        return ModelKind::Unknown;
    };
    EXACT_KINDS
        .iter()
        .find(|(p, id, _)| *p == provider && *id == selection.model)
        .map_or(ModelKind::Unknown, |(_, _, kind)| *kind)
}

#[cfg(test)]
mod tests;

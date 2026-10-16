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
use std::{collections::HashMap, sync::LazyLock};

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
        let split = CacheWriteSplit::of(reply);
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
                    Entry {
                        price: list_price(&row),
                        context_window: row.c.filter(|n| *n > 0),
                        max_output_tokens: row.o.filter(|n| *n > 0),
                        reasoning: row.r != 0,
                        vision: row.v != 0,
                        thinking: row.t,
                        efforts: row.e,
                        default_effort: row.d,
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

fn to_bundled(provider: &'static String, entry: &'static Entry) -> BundledModel {
    BundledModel {
        id: entry.id.clone(),
        provider: provider.as_str(),
        context_window: entry.context_window,
        max_output_tokens: entry.max_output_tokens,
        reasoning: entry.reasoning,
        vision: entry.vision,
        thinking: entry.thinking,
        thinking_efforts: entry.efforts.clone(),
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
    candidates
        .iter()
        .find_map(|id| models.index.get(id))
        .map(|&i| to_bundled(key, &models.entries[i]))
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
                .map(|entry| to_bundled(key, entry))
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
    mut metadata: ModelMetadata,
) -> ModelMetadata {
    metadata.kind = known_kind(selection, metadata.kind);
    if let Some(model) = selection_model(selection) {
        metadata.vision = metadata.vision.or(Some(model.vision));
        metadata.reasoning = metadata.reasoning.or(Some(model.reasoning));
        metadata.max_output_tokens = metadata.max_output_tokens.or(model.max_output_tokens);
        metadata.pricing = metadata
            .pricing
            .or_else(|| model.list_price.map(ListPrice::token_pricing));
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
mod tests {
    use super::*;

    #[test]
    fn exact_kinds_come_from_documented_ids_and_never_from_families() {
        let openai = |model: &str| ModelSelection {
            provider: "openai".into(),
            model: model.into(),
            base_url: Some("https://api.openai.com/v1".into()),
            ..Default::default()
        };
        for id in [
            "text-embedding-3-small",
            "text-embedding-3-large",
            "text-embedding-ada-002",
        ] {
            assert_eq!(
                known_kind(&openai(id), ModelKind::Unknown),
                ModelKind::Embedding
            );
        }
        // Family prefixes, unlisted ids and a live-reported purpose are not overridden by the table.
        assert_eq!(
            known_kind(&openai("text-embedding-9"), ModelKind::Unknown),
            ModelKind::Unknown
        );
        assert_eq!(
            known_kind(&openai("gpt-4o"), ModelKind::Unknown),
            ModelKind::Unknown
        );
        assert_eq!(
            known_kind(&openai("text-embedding-3-small"), ModelKind::Chat),
            ModelKind::Chat
        );
        // The same id on a local or unrelated endpoint is not the documented OpenAI model.
        let local = ModelSelection {
            provider: "openai".into(),
            model: "text-embedding-3-small".into(),
            base_url: Some("http://127.0.0.1:8000/v1".into()),
            ..Default::default()
        };
        assert_eq!(known_kind(&local, ModelKind::Unknown), ModelKind::Unknown);
    }
    fn reply(prompt: u64, completion: u64, read: Option<u64>, write: Option<u64>) -> Reply {
        Reply {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            cached_prompt_tokens: read,
            cache_creation_tokens: write,
            ..Reply::default()
        }
    }

    #[test]
    fn bundled_data_parses_and_registers_legacy_and_new_providers() {
        assert!(catalog().len() > 50);
        for id in [
            "gemini",
            "openai",
            "anthropic",
            "openrouter",
            "groq",
            "mistral",
            "deepseek",
        ] {
            assert!(builtin_provider(id).is_some_and(|p| p.listed), "{id}");
        }
        let (url, env, protocol) = provider_endpoint("together").unwrap();
        assert_eq!(
            (url, env, protocol),
            (
                "https://api.together.xyz/v1",
                Some("TOGETHER_API_KEY"),
                ApiProtocol::ChatCompletions
            )
        );
        assert_eq!(
            provider_endpoint("fireworks").unwrap().1,
            Some("FIREWORKS_API_KEY")
        );
        assert_eq!(
            provider_endpoint("anthropic").unwrap().0,
            "https://api.anthropic.com/v1"
        );
        assert!(builtin_provider("chatgpt").is_none() && builtin_provider("mlx").is_none());
        for provider in builtin_providers() {
            assert!(provider.base_url.starts_with("https://"), "{}", provider.id);
            assert!(
                provider.env_keys().next().is_some()
                    || crate::llm::oauth::flow(&provider.id).is_some(),
                "{}",
                provider.id
            );
            if provider.protocol == ApiProtocol::Anthropic {
                assert!(provider.base_url.ends_with("/v1"), "{}", provider.id);
            }
        }
    }

    #[test]
    fn first_party_rows_price_direct_sessions() {
        for (provider, model) in [
            ("anthropic", "claude-opus-4-6"),
            ("openai", "gpt-5.5"),
            ("gemini", "gemini-3-pro-preview"),
            ("gemini", "models/gemini-3-pro-preview"),
            ("chatgpt", "gpt-5.6-sol"),
        ] {
            let found =
                bundled_model(provider, model).unwrap_or_else(|| panic!("{provider}/{model}"));
            assert!(
                found.list_price.is_some() && found.context_window.is_some(),
                "{provider}/{model}"
            );
        }
        let opus = bundled_model("anthropic", "Claude-Opus-4-6").unwrap();
        assert_eq!(opus.thinking, Some(ThinkingMode::AnthropicAdaptive));
        assert!(opus.thinking_efforts.contains(&ReasoningEffort::Max));
        // $5/M input, $25/M output, $0.50/M cache reads, $6.25/M cache writes.
        let cost = opus
            .list_price
            .unwrap()
            .reply_cost(&reply(1_000_000, 100_000, Some(400_000), Some(100_000)), 0)
            .unwrap();
        assert!(
            (cost
                - (500_000.0 * 5.0 + 100_000.0 * 25.0 + 400_000.0 * 0.5 + 100_000.0 * 6.25) / 1e6)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn missing_usage_and_inconsistent_cache_counts_stay_unpriced() {
        let price = bundled_model("anthropic", "claude-opus-4-6")
            .unwrap()
            .list_price
            .unwrap();
        let mut partial = reply(10, 1, None, None);
        partial.completion_tokens = None;
        assert_eq!(price.reply_cost(&partial, 0), None);
        assert_eq!(price.reply_cost(&reply(10, 1, Some(11), None), 0), None);
    }

    #[test]
    fn cache_writes_price_per_ttl_bucket() {
        let price = ListPrice {
            base: Rates::new([1.0, 2.0, 0.1, 3.75]).unwrap(),
            long_context: None,
            schedule: None,
        };
        let mut split = reply(1_000_000, 0, None, Some(1_000_000));
        split.cache_creation_5m_tokens = Some(400_000);
        split.cache_creation_1h_tokens = Some(600_000);
        // 400k at the 5m rate (3.75/M) plus 600k at the 1h rate (2x input = 2.0/M).
        assert!((price.reply_cost(&split, 0).unwrap() - 2.7).abs() < 1e-9);
        // Without a split, the whole write prices at the default write rate.
        let flat = reply(1_000_000, 0, None, Some(1_000_000));
        assert!((price.reply_cost(&flat, 0).unwrap() - 3.75).abs() < 1e-9);
    }

    #[test]
    fn long_context_and_off_peak_schedules_follow_upstream_rules() {
        let price = ListPrice {
            base: Rates::new([1.0, 2.0, 0.1, 0.0]).unwrap(),
            long_context: Some(LongContext {
                threshold: 100,
                inclusive: false,
                rates: Rates::new([10.0, 20.0, 1.0, 0.0]).unwrap(),
            }),
            schedule: None,
        };
        assert!(
            (price
                .reply_cost(&reply(100, 1_000_000, None, None), 0)
                .unwrap()
                - (100.0 + 2_000_000.0) / 1e6)
                .abs()
                < 1e-12
        );
        assert!(
            (price
                .reply_cost(&reply(101, 1_000_000, None, None), 0)
                .unwrap()
                - (1010.0 + 20_000_000.0) / 1e6)
                .abs()
                < 1e-12
        );
        let scheduled = ListPrice {
            base: Rates::new([1.0, 2.0, 0.0, 0.0]).unwrap(),
            long_context: None,
            schedule: Some(Schedule {
                off_peak: 0.5,
                // Monday 01:00-04:00 UTC.
                peak: vec![(vec![1], 60, 240)],
                effective: vec![(10 * 86_400_000, Rates::new([4.0, 4.0, 0.0, 0.0]).unwrap())],
            }),
        };
        // 1970-01-05 was a Monday.
        let monday_2am = 4 * 86_400_000 + 2 * 3_600_000;
        let monday_5am = 4 * 86_400_000 + 5 * 3_600_000;
        let one_million = reply(1_000_000, 0, None, None);
        assert_eq!(scheduled.reply_cost(&one_million, monday_2am), Some(1.0));
        assert_eq!(scheduled.reply_cost(&one_million, monday_5am), Some(0.5));
        // From day 10 the replacement card applies (day 11 is a Monday, 02:00 is peak).
        assert_eq!(
            scheduled.reply_cost(&one_million, 11 * 86_400_000 + 2 * 3_600_000),
            Some(4.0)
        );
    }

    #[test]
    fn selectors_resolve_qualified_bare_vendor_and_fireworks_wire_ids() {
        assert_eq!(
            known_context_window("anthropic/claude-opus-4-6"),
            Some(1_000_000)
        );
        assert_eq!(
            model_capabilities("claude-opus-4-6").unwrap().provider,
            "anthropic"
        );
        assert!(model_capabilities("together/moonshotai/Kimi-K2.7-Code").is_some());
        assert_eq!(
            fireworks_public_id("accounts/fireworks/models/kimi-k2p6").as_deref(),
            Some("kimi-k2.6")
        );
        assert!(bundled_model("fireworks", "accounts/fireworks/models/kimi-k3").is_some());
        assert!(model_capabilities("definitely-not-a-model").is_none());
    }

    #[test]
    fn selection_pricing_follows_the_endpoint_origin() {
        let selection = |provider: &str, base: Option<&str>| ModelSelection {
            provider: provider.into(),
            model: "claude-opus-4-6".into(),
            base_url: base.map(str::to_owned),
            ..Default::default()
        };
        assert_eq!(
            selection_catalog_provider(&selection(
                "anthropic",
                Some("https://api.anthropic.com/v1")
            )),
            Some("anthropic")
        );
        assert_eq!(
            selection_catalog_provider(&selection("custom", Some("https://api.together.xyz/v1/"))),
            Some("together")
        );
        assert_eq!(
            selection_catalog_provider(&selection(
                "anthropic",
                Some("https://proxy.example.com/v1")
            )),
            None
        );
        assert_eq!(
            selection_catalog_provider(&selection("custom", Some("http://127.0.0.1:1234/v1"))),
            None
        );
        assert_eq!(selection_catalog_provider(&selection("mlx", None)), None);
        assert_eq!(
            selection_catalog_provider(&selection(
                "fireworks",
                Some("https://api.fireworks.ai/inference/v1")
            )),
            Some("fireworks")
        );
        assert_eq!(
            selection_catalog_provider(&selection(
                "firepass",
                Some("https://api.fireworks.ai/inference/v1")
            )),
            Some("firepass")
        );
    }
}

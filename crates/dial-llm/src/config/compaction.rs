//! Context compaction policy: thresholds, reserves, methods and their serialized forms.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DEFAULT_RESERVE_TOKENS: u32 = 16_384;

/// Windows up to this size get a share-of-window default reserve (when `reserve_tokens` is unset).
/// The fixed 16k default took half of a 32k local window before any history was kept.
pub const SMALL_WINDOW_TOKENS: u32 = 65_536;

const SMALL_WINDOW_RESERVE_PERCENT: u32 = 20;

const SMALL_WINDOW_RESERVE_FLOOR: u32 = 2_048;

/// Default verbatim tail; a configured value equal to this on a small window is scaled to fit the band.
pub const DEFAULT_KEEP_RECENT_TOKENS: u32 = 20_000;

/// Large-window auto trigger uses the whole reserve-subtracted window; speculation starts earlier.
pub const DEFAULT_THRESHOLD_PERCENT: u64 = 100;

/// Small windows retain extra headroom for estimation drift and less capable local models.
const SMALL_WINDOW_THRESHOLD_PERCENT: u64 = 80;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionMethod {
    Remote,
    Snapcompact,
    Handoff,
    Shake,
    Soft,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionStrategy {
    #[default]
    ContextFull,
    Handoff,
    Shake,
    Snapcompact,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeCompactionCapability {
    ResponsesV1,
    ResponsesV2,
    ResponsesV1V2,
    Anthropic,
    #[serde(rename = "none")]
    Disabled,
}

/// A number is a model policy base; `fN` is a fixed trigger; `N%` is a percentage.
/// Named-agent numbers are fixed triggers, not model policy bases.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompactionThreshold {
    Tokens(u64),
    Expression(String),
}

#[derive(Clone, Copy, Debug)]
pub enum ThresholdValue {
    Base(u64),
    Fixed(u64),
    Percent(f64),
}

impl CompactionThreshold {
    pub fn parse(&self, agent: bool) -> Result<ThresholdValue> {
        match self {
            Self::Tokens(n) => {
                ensure!(
                    *n > 0 && *n <= 9_007_199_254_740_991,
                    "compaction token threshold must be a positive safe integer"
                );
                Ok(if agent {
                    ThresholdValue::Fixed(*n)
                } else {
                    ThresholdValue::Base(*n)
                })
            }
            Self::Expression(text) => {
                let text = text.trim();
                if let Some(percent) = text.strip_suffix('%') {
                    ensure!(
                        !percent.is_empty()
                            && percent.chars().all(|c| c.is_ascii_digit() || c == '.'),
                        "invalid compaction percentage"
                    );
                    let value = percent
                        .parse::<f64>()
                        .context("invalid compaction percentage")?;
                    ensure!(
                        value.is_finite() && value > 0.0 && value <= 100.0,
                        "compaction percentage must be in (0, 100]"
                    );
                    return Ok(ThresholdValue::Percent(value));
                }
                if !agent && (text.starts_with('f') || text.starts_with('F')) {
                    let digits = &text[1..];
                    ensure!(
                        !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()),
                        "fixed compaction threshold must be f followed by a positive integer"
                    );
                    let value = digits
                        .parse::<u64>()
                        .context("invalid fixed compaction threshold")?;
                    ensure!(
                        value > 0 && value <= 9_007_199_254_740_991,
                        "fixed compaction threshold must be a positive safe integer"
                    );
                    return Ok(ThresholdValue::Fixed(value));
                }
                bail!(
                    "compaction threshold must be a positive integer, {}or a percentage in (0, 100]",
                    if agent { "" } else { "fN, " }
                );
            }
        }
    }
}

fn auto_deserialize<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input<T> {
        Value(T),
        Marker(String),
    }
    match Input::deserialize(d)? {
        Input::Value(value) => Ok(Some(value)),
        Input::Marker(marker) if marker == "auto" => Ok(None),
        Input::Marker(_) => Err(serde::de::Error::custom(
            "expected a number or the canonical 'auto' marker",
        )),
    }
}

fn auto_serialize<S: serde::Serializer, T: Serialize>(
    value: &Option<T>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(value) => value.serialize(s),
        None => s.serialize_str("auto"),
    }
}

fn inherit_string_deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = String::deserialize(d)?;
    Ok((value != "inherit").then_some(value))
}

fn inherit_string_serialize<S: serde::Serializer>(
    value: &Option<String>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    match value {
        Some(value) => value.serialize(s),
        None => s.serialize_str("inherit"),
    }
}

fn threshold_map_deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<BTreeMap<String, Option<CompactionThreshold>>, D::Error> {
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = BTreeMap<String, Option<CompactionThreshold>>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a threshold map with numeric, fN, percent, or inherit values")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, CompactionThreshold>()? {
                let reset = matches!(&value, CompactionThreshold::Expression(marker) if marker == "inherit");
                if values.insert(key, (!reset).then_some(value)).is_some() {
                    return Err(serde::de::Error::custom(
                        "duplicate compaction threshold selector",
                    ));
                }
            }
            Ok(values)
        }
    }
    d.deserialize_map(Entries)
}

fn threshold_map_serialize<S: serde::Serializer>(
    values: &BTreeMap<String, Option<CompactionThreshold>>,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    struct Entry<'a>(&'a Option<CompactionThreshold>);
    impl Serialize for Entry<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
            match self.0 {
                Some(value) => value.serialize(s),
                None => s.serialize_str("inherit"),
            }
        }
    }
    let mut map = s.serialize_map(Some(values.len()))?;
    for (key, value) in values {
        map.serialize_entry(key, &Entry(value))?;
    }
    map.end()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub strategy: CompactionStrategy,
    pub method_order: Vec<CompactionMethod>,
    #[serde(
        deserialize_with = "auto_deserialize",
        serialize_with = "auto_serialize"
    )]
    pub threshold_percent: Option<f64>,
    #[serde(
        deserialize_with = "auto_deserialize",
        serialize_with = "auto_serialize"
    )]
    pub threshold_tokens: Option<u64>,
    #[serde(
        deserialize_with = "auto_deserialize",
        serialize_with = "auto_serialize"
    )]
    pub reserve_tokens: Option<u32>,
    pub keep_recent_tokens: u32,
    pub mid_turn_enabled: bool,
    pub auto_continue: bool,
    pub remote_enabled: bool,
    #[serde(
        deserialize_with = "inherit_string_deserialize",
        serialize_with = "inherit_string_serialize"
    )]
    pub remote_endpoint: Option<String>,
    pub remote_streaming_v2_enabled: bool,
    pub v2_retained_message_budget: u32,
    #[serde(
        deserialize_with = "threshold_map_deserialize",
        serialize_with = "threshold_map_serialize"
    )]
    pub model_thresholds: BTreeMap<String, Option<CompactionThreshold>>,
    pub model_thresholds_enabled: bool,
    #[serde(
        deserialize_with = "threshold_map_deserialize",
        serialize_with = "threshold_map_serialize"
    )]
    pub agent_threshold_overrides: BTreeMap<String, Option<CompactionThreshold>>,
    pub native_models: BTreeMap<String, NativeCompactionCapability>,
    pub snapcompact: super::snapcompact::SnapcompactConfig,
    pub snapcompact_models: BTreeMap<String, super::snapcompact::ImageTokenization>,
    pub async_enabled: bool,
    pub idle_enabled: bool,
    pub idle_threshold_tokens: u64,
    pub idle_timeout_seconds: u64,
    pub handoff_save_to_disk: bool,
    /// Preserve sent history until context pressure requires pruning.
    pub preserve_prefix: bool,
    pub supersede_reads: bool,
    pub drop_useless: bool,
    pub experimental_context_management: bool,
    #[serde(
        deserialize_with = "inherit_string_deserialize",
        serialize_with = "inherit_string_serialize"
    )]
    pub summary_instructions: Option<String>,
    /// Opt-in switch to a larger-window model before compacting.
    pub promotion: PromotionConfig,
}

/// Context promotion: when a conversation is about to be compacted, or the provider rejects it for
/// context overflow, switch to a configured larger-window model first and compact only when no
/// target qualifies. Off by default. The switch lasts until the user changes models.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromotionConfig {
    pub enabled: bool,
    /// Ordered target selectors (`@role`, `provider/model` or a bare model on the current provider)
    /// keyed like `retry.fallback_chains`: `default`, an exact `provider/model`, or `provider/*`.
    pub targets: BTreeMap<String, Vec<String>>,
}

/// Most keys and most targets per key a promotion table may hold.
const MAX_PROMOTION_KEYS: usize = 128;
const MAX_PROMOTION_TARGETS: usize = 32;

impl PromotionConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.targets.len() <= MAX_PROMOTION_KEYS,
            "compaction.promotion.targets exceeds {MAX_PROMOTION_KEYS} keys"
        );
        for (key, chain) in &self.targets {
            ensure!(
                !key.trim().is_empty() && key.len() <= 512,
                "invalid compaction.promotion.targets key"
            );
            ensure!(
                !chain.is_empty(),
                "compaction.promotion.targets {key:?} must list at least one model"
            );
            ensure!(
                chain.len() <= MAX_PROMOTION_TARGETS,
                "compaction.promotion.targets {key:?} exceeds {MAX_PROMOTION_TARGETS} entries"
            );
            for selector in chain {
                ensure!(
                    !selector.trim().is_empty() && selector.trim().len() <= 512,
                    "invalid model selector in compaction.promotion.targets {key:?}"
                );
            }
        }
        Ok(())
    }
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strategy: CompactionStrategy::ContextFull,
            // A text summary is the default; Remote needs a declared native route and is skipped
            // otherwise. Model-free fallbacks (Shake, Snapcompact) only run when summarizing fails.
            // Snapcompact must not come before a summary method: it is image-only and also has no
            // background speculation, so leading with it made every vision-model compaction blocking.
            method_order: vec![
                CompactionMethod::Remote,
                CompactionMethod::Soft,
                CompactionMethod::Shake,
                CompactionMethod::Snapcompact,
                CompactionMethod::Handoff,
            ],
            threshold_percent: None,
            threshold_tokens: None,
            reserve_tokens: None,
            keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
            mid_turn_enabled: true,
            auto_continue: true,
            remote_enabled: true,
            remote_endpoint: None,
            remote_streaming_v2_enabled: true,
            v2_retained_message_budget: 64_000,
            model_thresholds: BTreeMap::new(),
            model_thresholds_enabled: true,
            agent_threshold_overrides: BTreeMap::new(),
            native_models: BTreeMap::new(),
            snapcompact: Default::default(),
            snapcompact_models: BTreeMap::new(),
            async_enabled: true,
            idle_enabled: false,
            idle_threshold_tokens: 200_000,
            idle_timeout_seconds: 300,
            handoff_save_to_disk: false,
            preserve_prefix: true,
            supersede_reads: false,
            drop_useless: false,
            experimental_context_management: false,
            summary_instructions: None,
            promotion: PromotionConfig::default(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct CompactionIdentity<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub agent: Option<&'a str>,
}

impl CompactionConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(n) = self.threshold_percent {
            ensure!(
                n.is_finite() && n > 0.0 && n <= 100.0,
                "compaction.threshold_percent must be in (0, 100]"
            );
        }
        if let Some(n) = self.threshold_tokens {
            ensure!(
                n > 0 && n <= 9_007_199_254_740_991,
                "compaction.threshold_tokens must be a positive safe integer"
            );
        }
        ensure!(
            self.reserve_tokens != Some(0),
            "compaction.reserve_tokens must be positive or omitted"
        );
        ensure!(
            self.v2_retained_message_budget > 0 && self.v2_retained_message_budget <= 64_000,
            "compaction.v2_retained_message_budget must be 1..64000"
        );
        ensure!(
            self.idle_threshold_tokens > 0 && self.idle_timeout_seconds > 0,
            "idle compaction threshold and delay must be positive"
        );
        let mut seen = Vec::new();
        for method in &self.method_order {
            ensure!(
                !seen.contains(method),
                "compaction.method_order contains a duplicate method"
            );
            seen.push(*method);
        }
        for (key, value) in &self.model_thresholds {
            validate_selector(key)?;
            if let Some(value) = value {
                value.parse(false)?;
            }
        }
        for key in self.native_models.keys() {
            validate_selector(key)?;
        }
        self.snapcompact.validate()?;
        self.promotion.validate()?;
        for (key, billing) in &self.snapcompact_models {
            validate_selector(key)?;
            billing.validate()?;
        }
        for (key, value) in &self.agent_threshold_overrides {
            ensure!(
                !key.trim().is_empty() && !key.contains('*'),
                "agent compaction thresholds require exact nonempty agent names"
            );
            if let Some(value) = value {
                value.parse(true)?;
            }
        }
        if let Some(endpoint) = &self.remote_endpoint {
            let url = url::Url::parse(endpoint).context("invalid compaction.remote_endpoint")?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "compaction.remote_endpoint must be an HTTP(S) URL without inline credentials, query, or fragment"
            );
        }
        if let Some(text) = &self.summary_instructions {
            ensure!(
                text.len() <= 64 * 1024,
                "compaction.summary_instructions exceeds 64 KiB"
            );
        }
        Ok(())
    }
    pub fn reserve(&self, window: u32) -> u32 {
        let reserve = self.fixed_or_proportional_reserve(window);
        // Small default windows reserve at most 20% (never below a 2k floor), so the reserve no longer
        // consumes half the window. Explicit `reserve_tokens` and large windows are unchanged, and the
        // reserve never grows past the value the 16k/15% rule already gave.
        if self.reserve_tokens.is_none() && window <= SMALL_WINDOW_TOKENS {
            reserve.min(
                (window.saturating_mul(SMALL_WINDOW_RESERVE_PERCENT) / 100)
                    .max(SMALL_WINDOW_RESERVE_FLOOR),
            )
        } else {
            reserve
        }
    }
    fn fixed_or_proportional_reserve(&self, window: u32) -> u32 {
        let proportional = (window.saturating_mul(15) / 100).max(1);
        let reserve = proportional.max(self.reserve_tokens.unwrap_or(DEFAULT_RESERVE_TOKENS));
        if reserve >= window
            || (self.reserve_tokens.is_none() && reserve >= window.saturating_sub(proportional))
        {
            proportional
        } else {
            reserve
        }
    }
    pub fn summary_budget(&self, window: u32) -> u32 {
        (self.reserve(window).saturating_mul(4) / 5).clamp(1, MAX_SUMMARY_TOKENS)
    }
    /// Verbatim-tail budget for a cut without an explicit per-call value. The default is scaled only
    /// on small windows so that the next request (`fixed_tokens` of system/tools, the summary budget,
    /// and the kept tail) stays within `continue_band`; otherwise the turn pauses after compaction.
    /// Worked example at 32k: reserve 6553, summary budget 5242, threshold 20972, band 16777, so the
    /// tail is 0.9 * (16777 - 5242) = 10381 < 20000 when fixed overhead is zero. At 64k the band
    /// (33751) already fits the default, so it is unchanged. Explicit values and windows above
    /// SMALL_WINDOW_TOKENS are returned as configured.
    pub fn keep_recent_budget(
        &self,
        identity: CompactionIdentity<'_>,
        window: u32,
        fixed_tokens: u64,
    ) -> Result<u64> {
        let configured = u64::from(self.keep_recent_tokens);
        if window > SMALL_WINDOW_TOKENS || self.keep_recent_tokens != DEFAULT_KEEP_RECENT_TOKENS {
            return Ok(configured);
        }
        let band = continue_band(self.effective_threshold(identity, window)?);
        let room = band
            .saturating_sub(fixed_tokens)
            .saturating_sub(u64::from(self.summary_budget(window)));
        // Keep 10% of the room free for estimate drift between the cut and the committed request.
        Ok(configured.min(room / 10 * 9))
    }
    pub fn effective_threshold(
        &self,
        identity: CompactionIdentity<'_>,
        window: u32,
    ) -> Result<u64> {
        ensure!(
            window > 1,
            "compaction needs a known context window greater than one token"
        );
        let mut fixed = self.threshold_tokens;
        let mut percent = self.threshold_percent;
        let mut base = u64::from(window);
        let agent = identity
            .agent
            .and_then(|name| self.agent_threshold_overrides.get(name))
            .and_then(Option::as_ref);
        let model = if agent.is_none() && self.model_thresholds_enabled {
            let key = format!("{}/{}", identity.provider, identity.model);
            self.model_thresholds
                .get(&key)
                .and_then(Option::as_ref)
                .or_else(|| {
                    self.model_thresholds
                        .iter()
                        .filter(|(selector, value)| {
                            value.is_some()
                                && selector.ends_with('*')
                                && key.starts_with(&selector[..selector.len() - 1])
                        })
                        .max_by_key(|(selector, _)| selector.len())
                        .and_then(|(_, value)| value.as_ref())
                })
        } else {
            None
        };
        if let Some(value) = agent.or(model) {
            match value.parse(agent.is_some())? {
                ThresholdValue::Fixed(n) => {
                    fixed = Some(n);
                    percent = None;
                }
                ThresholdValue::Percent(n) => {
                    fixed = None;
                    percent = Some(n);
                }
                ThresholdValue::Base(n) => {
                    fixed = None;
                    base = base.min(n);
                }
            }
        }
        if let Some(n) = fixed {
            return Ok(n.max(1).min(u64::from(window) - 1));
        }
        if let Some(n) = percent {
            return Ok(((base as f64 * n / 100.0).floor() as u64)
                .max(1)
                .min(base.saturating_sub(1)));
        }
        // Prepare summaries ahead of this boundary, but keep large-window history until the reserve
        // is reached. Small windows retain their conservative extra margin.
        let usable = base
            .saturating_sub(u64::from(
                self.reserve(base.min(u64::from(u32::MAX)) as u32),
            ))
            .min(base.saturating_sub(1));
        let percent = if base <= u64::from(SMALL_WINDOW_TOKENS) {
            SMALL_WINDOW_THRESHOLD_PERCENT
        } else {
            DEFAULT_THRESHOLD_PERCENT
        };
        Ok((usable.saturating_mul(percent) / 100).max(1))
    }
    pub fn should_compact(
        &self,
        identity: CompactionIdentity<'_>,
        window: u32,
        tokens: u64,
        mid_turn: bool,
    ) -> Result<bool> {
        Ok(self.enabled
            && self.strategy != CompactionStrategy::Off
            && (!mid_turn || identity.agent.is_some() || self.mid_turn_enabled)
            && (self.strategy != CompactionStrategy::ContextFull || !self.method_order.is_empty())
            && tokens > self.effective_threshold(identity, window)?)
    }
    pub fn native_capability(
        &self,
        identity: CompactionIdentity<'_>,
    ) -> Option<NativeCompactionCapability> {
        match_selector(&self.native_models, identity).copied()
    }
    pub fn methods(&self, options: &CompactionOptions) -> Vec<CompactionMethod> {
        if let Some(method) = options.method {
            return if method == CompactionMethod::Remote && !options.require_method {
                vec![method, CompactionMethod::Soft]
            } else {
                vec![method]
            };
        }
        match options.strategy.unwrap_or(self.strategy) {
            CompactionStrategy::ContextFull => self.method_order.clone(),
            CompactionStrategy::Handoff => vec![CompactionMethod::Handoff],
            CompactionStrategy::Shake => vec![CompactionMethod::Shake],
            CompactionStrategy::Snapcompact => vec![CompactionMethod::Snapcompact],
            CompactionStrategy::Off => Vec::new(),
        }
    }
    /// Unsupported native compaction is not a failed recovery attempt. Automatic selection
    /// skips it; an explicitly requested method still runs and explains why it is unavailable.
    pub fn available_methods(
        &self,
        options: &CompactionOptions,
        native_supported: bool,
    ) -> Vec<CompactionMethod> {
        self.methods(options)
            .into_iter()
            .filter(|method| {
                *method != CompactionMethod::Remote
                    || options.method == Some(CompactionMethod::Remote)
                    || options.require_method
                    || self.remote_enabled && (self.remote_endpoint.is_some() || native_supported)
            })
            .collect()
    }
}

fn validate_selector(selector: &str) -> Result<()> {
    let (provider, model) = selector
        .split_once('/')
        .context("compaction model selector must be provider/model or a trailing-* prefix")?;
    ensure!(
        !provider.is_empty()
            && !model.is_empty()
            && selector.find('*').is_none_or(|i| i == selector.len() - 1)
            && !selector.chars().any(char::is_whitespace),
        "invalid compaction model selector {selector}"
    );
    Ok(())
}

pub fn match_selector<'a, T>(
    values: &'a BTreeMap<String, T>,
    identity: CompactionIdentity<'_>,
) -> Option<&'a T> {
    let key = format!("{}/{}", identity.provider, identity.model);
    values.get(&key).or_else(|| {
        values
            .iter()
            .filter(|(selector, _)| {
                selector.ends_with('*') && key.starts_with(&selector[..selector.len() - 1])
            })
            .max_by_key(|(selector, _)| selector.len())
            .map(|(_, value)| value)
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionReason {
    #[default]
    Manual,
    Threshold,
    MidTurn,
    Overflow,
    Idle,
    Speculative,
    Handoff,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionOptions {
    pub instructions: Option<String>,
    pub method: Option<CompactionMethod>,
    pub strategy: Option<CompactionStrategy>,
    pub keep_recent_tokens: Option<u32>,
    pub suppress_continuation: bool,
    pub require_method: bool,
    pub reason: CompactionReason,
}

impl CompactionOptions {
    pub fn from_args(args: &str) -> Result<Self> {
        let text = args.trim();
        let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let method = match first.to_ascii_lowercase().as_str() {
            "soft" => Some(CompactionMethod::Soft),
            "remote" => Some(CompactionMethod::Remote),
            "snapcompact" => Some(CompactionMethod::Snapcompact),
            "handoff" => Some(CompactionMethod::Handoff),
            "shake" => Some(CompactionMethod::Shake),
            _ => None,
        };
        let instructions = if method.is_some() { rest.trim() } else { text };
        ensure!(
            !matches!(
                method,
                Some(CompactionMethod::Snapcompact | CompactionMethod::Shake)
            ) || instructions.is_empty(),
            "/compact snapcompact and shake do not take focus instructions"
        );
        Ok(Self {
            method,
            instructions: (!instructions.is_empty()).then(|| instructions.to_owned()),
            ..Self::default()
        })
    }
}

pub const MAX_SUMMARY_TOKENS: u32 = DEFAULT_RESERVE_TOKENS;

/// Largest post-compaction size that may continue automatically. It must sit clearly below the
/// trigger so the next step does not re-compact at once, but a fixed 80% band paused long autonomous
/// turns after compactions that had already made a full speculation lead of room.
pub fn continue_band(threshold: u64) -> u64 {
    (threshold.saturating_mul(4) / 5).max(threshold.saturating_sub(speculation_lead(threshold)))
}

pub const SPECULATION_LEAD_MIN_TOKENS: u64 = 8_192;

pub(crate) const SPECULATION_LEAD_MAX_TOKENS: u64 = 32_000;

pub fn speculation_lead(threshold: u64) -> u64 {
    (threshold / 8).clamp(SPECULATION_LEAD_MIN_TOKENS, SPECULATION_LEAD_MAX_TOKENS)
}

#[cfg(test)]
mod tests;

//! Resolved provider policy. Transport mechanics consume these decisions; provider descriptors
//! and exact-model catalog records remain the source of capabilities, never name heuristics.
use super::{
    catalog::ModelMetadata,
    model_config::ApiProtocol,
    profiles::{ModelProfile, StructuredOutput},
};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::LazyLock};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderPolicy {
    protocol: Option<ApiProtocol>,
    service_tiers: Vec<String>,
    fast_tier: Option<String>,
    structured_output: Option<StructuredOutput>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policies {
    schema_version: u32,
    default_image_edge: u32,
    providers: BTreeMap<String, ProviderPolicy>,
}
static POLICIES: LazyLock<Policies> = LazyLock::new(|| {
    let policies: Policies = serde_json::from_str(include_str!("policy.json"))
        .expect("bundled provider policy must be valid");
    assert_eq!(
        policies.schema_version, 1,
        "unsupported provider policy version"
    );
    assert!(policies.default_image_edge > 0);
    policies
});
pub fn protocol(provider: &str) -> ApiProtocol {
    POLICIES
        .providers
        .get(provider)
        .and_then(|policy| policy.protocol)
        .or_else(|| super::catalog::builtin_provider(provider).map(|provider| provider.protocol))
        .unwrap_or(ApiProtocol::ChatCompletions)
}
pub fn service_tier<'a>(provider: &str, tier: &'a str) -> Option<&'a str> {
    POLICIES
        .providers
        .get(provider)
        .is_some_and(|policy| policy.service_tiers.iter().any(|allowed| allowed == tier))
        .then_some(tier)
}
pub fn fast_mode(provider: &str, tier: &str) -> bool {
    POLICIES
        .providers
        .get(provider)
        .and_then(|policy| policy.fast_tier.as_deref())
        == Some(tier)
}
pub fn structured_output(provider: &str) -> StructuredOutput {
    POLICIES
        .providers
        .get(provider)
        .and_then(|policy| policy.structured_output)
        .unwrap_or(StructuredOutput::Disabled)
}
pub fn image_edge() -> u32 {
    POLICIES.default_image_edge
}

/// One resolution used by request budgeting and capability consumers. Operator profiles take
/// precedence over live metadata, which takes precedence over exact catalog records.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub vision: Option<bool>,
    pub tools: Option<bool>,
}
impl ResolvedModel {
    pub fn resolve(
        provider: &str,
        model: &str,
        profile: Option<&ModelProfile>,
        metadata: &ModelMetadata,
        context_fallback: u32,
    ) -> Self {
        let exact = super::catalog::bundled_model(provider, model);
        let context_window = profile
            .and_then(|profile| profile.context_window)
            .or_else(|| (context_fallback > 0).then_some(context_fallback))
            .or_else(|| exact.as_ref().and_then(|model| model.context_window))
            .filter(|window| *window > 0)
            .unwrap_or(context_fallback.max(1));
        let max_output_tokens = profile
            .and_then(|profile| profile.max_tokens)
            .filter(|limit| *limit > 0)
            .or(metadata.max_output_tokens)
            .or_else(|| exact.as_ref().and_then(|model| model.max_output_tokens))
            .filter(|limit| *limit > 0)
            .unwrap_or_else(|| (context_window / 8).clamp(1024, 8192));
        Self {
            context_window,
            max_output_tokens,
            vision: profile
                .and_then(|profile| profile.capabilities.vision)
                .or(metadata.vision)
                .or_else(|| exact.as_ref().map(|model| model.vision)),
            tools: profile
                .and_then(|profile| profile.capabilities.tools)
                .or(metadata.tools)
                .or_else(|| exact.as_ref().and_then(|model| model.metadata().tools)),
        }
    }
    pub fn output_budget(&self, requested: u32) -> u32 {
        if requested == 0 {
            self.max_output_tokens
        } else {
            requested
        }
    }
}

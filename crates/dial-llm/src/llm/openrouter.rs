//! Explicit OpenRouter provider preferences. No inferred routing or arbitrary wire extensions.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Attribute OpenRouter should prioritize instead of its default balanced routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    Price,
    Latency,
    Throughput,
}

/// Whether eligible upstream providers may collect request data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataCollection {
    Allow,
    Deny,
}

/// Hard price ceilings: prompt/completion in USD per million tokens, request/image per unit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaxPrice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<f64>,
}

/// Optional exact-profile preferences serialized as OpenRouter's `provider` object on every attempt.
/// Absence leaves account/provider defaults intact; explicit false values remain explicit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<Sort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_collection: Option<DataCollection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_price: Option<MaxPrice>,
}

impl Routing {
    /// Validate controls without consulting a potentially stale upstream provider catalog.
    pub fn validate(&self, provider: &str) -> Result<()> {
        ensure!(
            provider == "openrouter",
            "openrouter_routing requires the openrouter provider"
        );
        for (name, list) in [
            ("order", &self.order),
            ("only", &self.only),
            ("ignore", &self.ignore),
        ] {
            if let Some(list) = list {
                ensure!(
                    !list.is_empty(),
                    "openrouter_routing.{name} must not be empty"
                );
                let mut seen = std::collections::HashSet::new();
                for slug in list {
                    ensure!(
                        slug.split('/').all(|part| !part.is_empty()
                            && part
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric()
                                    || matches!(b, b'-' | b'_' | b'.'))),
                        "openrouter_routing.{name} requires provider slugs without whitespace or empty segments"
                    );
                    ensure!(
                        seen.insert(slug),
                        "openrouter_routing.{name} contains a duplicate provider slug"
                    );
                }
            }
        }
        if let Some(price) = &self.max_price {
            let prices = [price.prompt, price.completion, price.request, price.image];
            ensure!(
                prices.iter().any(Option::is_some),
                "openrouter_routing.max_price requires at least one ceiling"
            );
            ensure!(
                prices
                    .into_iter()
                    .flatten()
                    .all(|p| p.is_finite() && p >= 0.0),
                "openrouter_routing.max_price ceilings must be finite and nonnegative"
            );
        }
        Ok(())
    }
}

/// Both API transports apply the same validated policy; retries never strip or relax it.
pub(super) fn apply(body: &mut Value, profile: &super::profiles::ModelProfile) -> Result<()> {
    if let Some(routing) = &profile.openrouter_routing {
        routing.validate(&profile.provider)?;
        let preferences = serde_json::to_value(routing)?;
        if preferences
            .as_object()
            .is_some_and(|object| !object.is_empty())
        {
            body["provider"] = preferences;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

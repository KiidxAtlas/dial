//! Shared service-tier change planning for every frontend.
use super::{settings_store::Change, types::Config};
use anyhow::{Result, bail};

/// Provider families whose `priority` tier Dial sends (`/fast`).
const PRIORITY_PROVIDERS: &[&str] = &[
    "openai",
    "openai-codex",
    "chatgpt",
    "anthropic",
    "google",
    "gemini",
    "google-vertex",
];
/// Provider families with OpenAI's `ultrafast` tier.
const ULTRAFAST_PROVIDERS: &[&str] = &["openai", "openai-codex"];
/// Provider families with the `flex` tier (`/slow`). Anthropic's low-priority lane is not supported.
const FLEX_PROVIDERS: &[&str] = &["openai", "openai-codex", "chatgpt", "google", "gemini"];

/// A `/fast` or `/slow` outcome: the note to show and the settings writes it needs.
pub struct TierPlan {
    pub note: String,
    pub changes: Vec<Change>,
}

pub fn set_value(path: &str, value: toml::Value) -> Change {
    Change::Set {
        path: path.into(),
        value,
    }
}

fn tier_plan(note: impl Into<String>, changes: Vec<Change>) -> TierPlan {
    TierPlan {
        note: note.into(),
        changes,
    }
}

/// `/fast` owns `fast_mode` and the explicit `service_tier` fast values; it never touches `slow_mode`.
pub fn fast_plan(config: &Config, provider: &str, args: &str) -> Result<TierPlan> {
    let tier = config.service_tier.as_str();
    let fast_tier = matches!(tier, "priority" | "ultrafast");
    let fast_on = config.fast_mode || fast_tier;
    let status = match tier {
        "ultrafast" => "ultra",
        _ if fast_on => "on",
        _ => "off",
    };
    let target = match args.trim() {
        "status" => return Ok(tier_plan(format!("Fast mode is {status}."), vec![])),
        "" | "toggle" if fast_on => "off",
        "" | "toggle" | "on" => "on",
        "ultra" | "ultrafast" => "ultra",
        "off" => "off",
        other => bail!("Usage: /fast [on|ultra|off|status] (got {other:?})"),
    };
    match target {
        "off" => {
            let mut changes = Vec::new();
            if config.fast_mode {
                changes.push(set_value("fast_mode", toml::Value::Boolean(false)));
            }
            if fast_tier {
                changes.push(set_value(
                    "service_tier",
                    toml::Value::String("none".into()),
                ));
            }
            Ok(tier_plan("Fast mode disabled.", changes))
        }
        "on" => {
            if !PRIORITY_PROVIDERS.contains(&provider) {
                return Ok(tier_plan(
                    "Fast mode is unavailable for the current model.",
                    vec![],
                ));
            }
            let mut changes = vec![set_value("fast_mode", toml::Value::Boolean(true))];
            if tier != "none" {
                changes.push(set_value(
                    "service_tier",
                    toml::Value::String("none".into()),
                ));
            }
            Ok(tier_plan("Fast mode enabled.", changes))
        }
        _ => {
            if !ULTRAFAST_PROVIDERS.contains(&provider) {
                return Ok(tier_plan(
                    "Ultrafast is unavailable for the current model.",
                    vec![],
                ));
            }
            Ok(tier_plan(
                "Ultrafast mode enabled.",
                vec![set_value(
                    "service_tier",
                    toml::Value::String("ultrafast".into()),
                )],
            ))
        }
    }
}

/// `/slow` owns `slow_mode` only; it never touches `fast_mode` or `service_tier`.
pub fn slow_plan(config: &Config, provider: &str, args: &str) -> Result<TierPlan> {
    let slow_on = config.slow_mode;
    let arg = args.trim();
    match arg {
        "status" => Ok(tier_plan(
            if slow_on {
                "Slow mode is on (flex tier)."
            } else {
                "Slow mode is off."
            },
            vec![],
        )),
        "" | "toggle" | "on" | "off" => {
            let enable = match arg {
                "on" => true,
                "off" => false,
                _ => !slow_on,
            };
            if !enable {
                let changes = if slow_on {
                    vec![set_value("slow_mode", toml::Value::Boolean(false))]
                } else {
                    vec![]
                };
                return Ok(tier_plan("Slow mode off.", changes));
            }
            if !FLEX_PROVIDERS.contains(&provider) {
                return Ok(tier_plan(
                    format!(
                        "Slow mode is unavailable for {provider}: Dial implements /slow as the flex tier \
                         on OpenAI and Google models; Anthropic's subscription low-priority lane is not ported."
                    ),
                    vec![],
                ));
            }
            Ok(tier_plan(
                "Slow mode on: requests use the flex tier (lower cost, higher latency).",
                vec![set_value("slow_mode", toml::Value::Boolean(true))],
            ))
        }
        other => bail!("Usage: /slow [on|off|status] (got {other:?})"),
    }
}

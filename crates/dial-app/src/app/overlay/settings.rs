//! Settings overlay hints, setup offers and value choices for embedding, local and chat models.

use crate::config::schema::{ModelUse, Names, Source};
use crate::config::types::Config;
use crate::llm::catalog;
use crate::llm::model_config::ModelSelection;
use crate::ui::overlay::settings::{ChoiceAction, SettingsOverlay, ValueChoice};
use std::collections::BTreeMap;

/// Read-only findings beside the settings rows that depend on installed programs.
pub(in crate::app) fn apply_settings_hints(overlay: &mut SettingsOverlay, effective: &Config) {
    for (path, hint) in crate::voice::settings_hints(effective) {
        overlay.set_hint(path, hint);
    }
    for (path, hint) in feature_readiness_hints(effective) {
        overlay.set_hint(path, hint);
    }
}

/// Prerequisite evidence only. In particular, configured models/credentials do not prove a
/// service connection works. Browsing this projection never starts a process or network check.
fn feature_readiness_hints(config: &Config) -> Vec<(&'static str, String)> {
    let (blocking, optional) = crate::setup::memory::backend_missing(config, config.memory.backend);
    let memory = if let Some(missing) = blocking.first() {
        format!("Needs setup · {missing}")
    } else if config.memory.backend == crate::config::memory_backend::MemoryBackendId::Hindsight {
        "Unknown · service configured; connection has not been tested here".into()
    } else {
        crate::setup::memory::status_summary(&blocking, &optional)
    };
    let memory_hint = format!("{memory} · /setup memory");
    let mut hints = vec![
        ("memory.backend", memory_hint.clone()),
        ("memory.enabled", memory_hint),
    ];
    let laya = if config.laya.engine != crate::config::laya::DecisionEngineType::Laya {
        "Unsupported here · configured decision engine is not implemented".to_owned()
    } else if crate::config::laya::expected_checkpoint(&config.laya).is_none() {
        "Needs setup · no decision checkpoint configured or installed".to_owned()
    } else {
        // A configured path/repository is not evidence that a service is running or ready.
        // Only an explicit owner health check can establish that; Settings never probes it.
        "Unknown · checkpoint selected; service health not observed here".to_owned()
    };
    for path in ["laya.enabled", "laya.auto_start"] {
        hints.push((
            path,
            format!("{laya} · /laya status; dial laya install --checkpoint PATH for setup"),
        ));
    }
    for (path, enable, value) in [
        (
            "voice.tts.backend",
            "voice.tts.auto_speak",
            serde_json::to_value(config.voice.tts.backend).unwrap_or_default(),
        ),
        (
            "voice.stt.backend",
            "voice.stt.enabled",
            serde_json::to_value(config.voice.stt.backend).unwrap_or_default(),
        ),
    ] {
        let value = value.as_str().unwrap_or("unknown");
        let readiness = crate::setup::voice::ready(config, path, value);
        let status = match (value, readiness) {
            ("mlx-whisper", _) if !cfg!(all(target_os = "macos", target_arch = "aarch64")) => {
                "Unsupported here · mlx-whisper requires Apple Silicon macOS".into()
            }
            ("unknown", _) => "Unknown · backend could not be read".into(),
            ("auto", _) => "Unknown · automatic backend is chosen when used".into(),
            ("openai", Ok(())) => {
                "Unknown · credentials available; connection not tested here".into()
            }
            ("openai", Err(_)) => {
                "Needs sign-in · configure voice credentials in /providers".into()
            }
            (_, Ok(())) => "Ready · local backend prerequisites found (not an audio test)".into(),
            (_, Err(error)) => format!("Needs setup · {error:#}"),
        };
        let destination = crate::setup::voice::recipe_for(path, value)
            .map_or_else(|| "/settings → Voice".into(), |id| format!("/setup {id}"));
        let hint = format!("{status} · {destination}");
        hints.push((path, hint.clone()));
        hints.push((enable, hint));
    }
    hints
}

/// Missing-dependency offers for the settings overlay, keyed by the setting path each one serves.
pub(in crate::app) fn setting_setup_offers(
    user_dir: &std::path::Path,
    config: &crate::config::Config,
) -> std::collections::BTreeMap<String, crate::ui::overlay::settings::SetupOffer> {
    let env = crate::setup::deps::Env::current(user_dir);
    let mut offers: std::collections::BTreeMap<String, crate::ui::overlay::settings::SetupOffer> =
        crate::setup::deps::run::setting_offers(
            &env,
            &config.voice.tts.backend,
            &config.voice.stt.backend,
        )
        .into_iter()
        .map(|offer| {
            (
                offer.path.clone(),
                crate::ui::overlay::settings::SetupOffer {
                    id: offer.id.to_owned(),
                    title: offer.title.to_owned(),
                    preview: offer.preview,
                },
            )
        })
        .collect();
    // The embedding model row offers a local model until one is configured.
    if let Some(offer) = crate::setup::embeddings::setting_offer(user_dir, config) {
        offers.insert(
            offer.path.to_owned(),
            crate::ui::overlay::settings::SetupOffer {
                id: offer.id,
                title: offer.title,
                preview: offer.preview,
            },
        );
    }
    offers
}

/// Discovered models the Settings Embedding model row offers for the `embedding` role.
pub(in crate::app) fn settings_embedding_models(
    discovery: Option<&catalog::Discovery>,
) -> Vec<crate::ui::overlay::settings::EmbeddingModel> {
    discovery
        .map(crate::ui::overlay::models::embedding_candidates)
        .unwrap_or_default()
        .into_iter()
        .map(
            |(entry, classified)| crate::ui::overlay::settings::EmbeddingModel {
                selection: entry.selection,
                label: entry.label,
                classified,
            },
        )
        .collect()
}

/// One setup offer per curated embedding model, so Ctrl+L sets up the model picked on the row.
pub(in crate::app) fn embedding_setup_offers(
    user_dir: &std::path::Path,
) -> std::collections::BTreeMap<String, crate::ui::overlay::settings::SetupOffer> {
    crate::setup::embeddings::catalog_offers(user_dir)
        .into_iter()
        .zip(crate::setup::embeddings::CATALOG)
        .map(|(offer, model)| {
            (
                model.name.to_owned(),
                crate::ui::overlay::settings::SetupOffer {
                    id: offer.id,
                    title: offer.title,
                    preview: offer.preview,
                },
            )
        })
        .collect()
}

/// The values each sourced setting offers in its picker: discovered and downloaded models, the
/// curated local models (offered as downloads when missing) and configured names. Choosing one
/// never downloads without the picker's confirmation; every setting stays open to typed values.
pub(in crate::app) fn settings_value_choices(
    config: &Config,
    discovery: Option<&catalog::Discovery>,
    user_dir: &std::path::Path,
) -> BTreeMap<String, Vec<ValueChoice>> {
    let models = match discovery {
        Some(discovery) => discovery.models.clone(),
        None => catalog::downloaded_local_models(user_dir),
    };
    let mut choices = BTreeMap::new();
    for def in crate::config::schema::registry() {
        if matches!(def.path, "voice.tts.backend" | "voice.stt.backend") {
            choices.insert(def.path.to_owned(), voice_choices(def, config, user_dir));
            continue;
        }
        let offered = match def.source() {
            Some(Source::Model(use_)) => model_choices(def, use_, config, &models),
            Some(Source::Names(names)) => name_choices(names, config, discovery),
            _ => continue,
        };
        if !offered.is_empty() {
            choices.insert(def.path.to_owned(), offered);
        }
    }
    choices
}

fn voice_choices(
    def: &crate::config::schema::SettingDef,
    config: &Config,
    user_dir: &std::path::Path,
) -> Vec<ValueChoice> {
    let crate::config::schema::Kind::Enum(options) = def.kind else {
        return Vec::new();
    };
    let env = crate::setup::deps::Env::current(user_dir);
    options
        .iter()
        .map(|(value, label)| {
            let ready = crate::setup::voice::ready(config, def.path, value);
            let action = match ready {
                Ok(()) => ChoiceAction::Set,
                // Auto is a policy, not a particular program to install.
                Err(_) if *value == "auto" => ChoiceAction::Set,
                Err(error) => match crate::setup::voice::recipe_for(def.path, value) {
                    Some(id) => ChoiceAction::Install {
                        preview: format!("{}\n{}", crate::setup::deps::run::preview_text(id, &env), match id {
                            "piper" => "Also prepares the en_US-lessac-medium voice model and matching JSON when no complete voice model is configured.",
                            "mlx-whisper" => "Also prepares the configured MLX Whisper model in the Hugging Face cache when needed.",
                            _ => "Keeps existing configured models when usable.",
                        }),
                    },
                    None => ChoiceAction::Unavailable {
                        reason: format!("{error:#}"),
                    },
                },
            };
            ValueChoice {
                value: (*value).to_owned(),
                label: (*label).to_owned(),
                action,
            }
        })
        .collect()
}

/// A downloaded MLX checkpoint's value: its Hugging Face repository id when it came from that
/// cache (portable across machines), otherwise its path.
fn local_value(entry: &catalog::ModelEntry) -> String {
    if entry.source.starts_with("Hugging Face cache") {
        entry.label.clone()
    } else {
        entry.selection.model.clone()
    }
}

fn model_choices(
    def: &crate::config::schema::SettingDef,
    use_: ModelUse,
    config: &Config,
    models: &[catalog::ModelEntry],
) -> Vec<ValueChoice> {
    let set = |value: String, label: String| ValueChoice {
        value,
        label,
        action: ChoiceAction::Set,
    };
    let mut out: Vec<ValueChoice> = Vec::new();
    let mut push = |choice: ValueChoice| {
        if !out.iter().any(|known| known.value == choice.value) {
            out.push(choice);
        }
    };
    match use_ {
        ModelUse::LocalMlx => {
            let local: Vec<&catalog::ModelEntry> = models
                .iter()
                .filter(|entry| {
                    entry.metadata.format == Some(crate::llm::local_format::LocalFormat::Mlx)
                })
                .collect();
            // A draft model must share the main model's tokenizer; those come first.
            if def.path == "local_runtime.draft_model" {
                for path in crate::llm::local_server::draft_candidates(&config.model) {
                    let value = local
                        .iter()
                        .find(|entry| entry.selection.model == path)
                        .map_or_else(|| path.clone(), |entry| local_value(entry));
                    push(set(
                        value.clone(),
                        format!("{value} · same tokenizer as the main model"),
                    ));
                }
            }
            // Suggested models: saved at once when downloaded, otherwise offered as a download.
            if let crate::config::schema::Kind::Choice(options) = def.kind {
                for (value, label) in options.iter().filter(|(value, _)| !value.is_empty()) {
                    let downloaded = local.iter().any(|entry| local_value(entry) == *value);
                    push(ValueChoice {
                        value: (*value).to_owned(),
                        label: (*label).to_owned(),
                        action: if downloaded {
                            ChoiceAction::Set
                        } else {
                            ChoiceAction::Download
                        },
                    });
                }
            }
            for entry in local {
                let value = local_value(entry);
                push(set(value.clone(), format!("{value} · downloaded")));
            }
        }
        ModelUse::Chat => {
            // The main model is assigned through the canonical selection writer, which prepares
            // local weights and keeps provider and profile consistent.
            for entry in models.iter().filter(|entry| entry.supports_chat()) {
                push(ValueChoice {
                    value: entry.selection.model.clone(),
                    label: format!("{} · {}", entry.display_provider(), entry.label),
                    action: ChoiceAction::Assign {
                        role: "main".to_owned(),
                        selection: entry.selection.clone(),
                    },
                });
            }
        }
        ModelUse::QualifiedChat => {
            for entry in models.iter().filter(|entry| entry.supports_chat()) {
                let value = qualified(&entry.selection);
                push(set(value.clone(), format!("{value} · {}", entry.label)));
            }
        }
        ModelUse::Image => {
            for entry in models.iter().filter(|entry| entry.supports_images()) {
                push(set(
                    entry.selection.model.clone(),
                    format!("{} · {}", entry.display_provider(), entry.label),
                ));
            }
        }
    }
    out
}

fn qualified(selection: &ModelSelection) -> String {
    if selection.provider.is_empty() {
        selection.model.clone()
    } else {
        format!("{}/{}", selection.provider, selection.model)
    }
}

fn name_choices(
    names: Names,
    config: &Config,
    discovery: Option<&catalog::Discovery>,
) -> Vec<ValueChoice> {
    let mut values: Vec<String> = match names {
        Names::Providers => config
            .providers
            .keys()
            .cloned()
            .chain(
                discovery
                    .into_iter()
                    .flat_map(|discovery| discovery.providers.iter().map(|p| p.id.clone())),
            )
            .collect(),
        Names::ModelProfiles => config.model_profiles.keys().cloned().collect(),
    };
    values.sort();
    values.dedup();
    values
        .into_iter()
        .map(|value| ValueChoice {
            label: value.clone(),
            value,
            action: ChoiceAction::Set,
        })
        .collect()
}

#[cfg(test)]
mod tests;

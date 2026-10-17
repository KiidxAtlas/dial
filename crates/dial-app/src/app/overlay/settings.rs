//! Settings overlay hints and setup offers for embedding and local models.

use crate::config::types::Config;
use crate::llm::catalog;
use crate::ui::overlay::settings::SettingsOverlay;

/// Read-only findings beside the settings rows that depend on installed programs.
pub(in crate::app) fn apply_settings_hints(overlay: &mut SettingsOverlay, effective: &Config) {
    for (path, hint) in crate::voice::settings_hints(effective) {
        overlay.set_hint(path, hint);
    }
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

//! Learning and LoRA status for the status line and the improve commands.

use crate::agent;
use crate::app::core::App;
use crate::app::git::StatusCache;
use crate::app::shell::STATUS_TTL;
use crate::config::types::Config;
use crate::learn::auto;
use crate::llm::catalog;
use crate::storage::store::Store;
use crate::ui::status::Lora;
use serde_json::Value;
use std::path::Path;

#[derive(Default)]
pub(super) struct Learning {
    pub(super) cache: StatusCache<(Option<i64>, String, Lora)>,
    pub(super) state_dir: Option<std::path::PathBuf>,
}

impl Learning {
    pub(super) fn read(&mut self, app: &App) {
        let state_dir = app.config.state_dir.clone();
        if self.state_dir.as_ref() != Some(&state_dir) {
            self.cache = StatusCache::default();
            self.state_dir = Some(state_dir.clone());
        }
        let config = app.reload().unwrap_or_else(|_| app.config.clone());
        let available = true;
        self.cache.refresh(STATUS_TTL, move || {
            let store = available
                .then(|| Store::open_read_only(&state_dir).ok())
                .flatten();
            let label = improvement_label(&config, store.as_ref());
            let active = store.as_ref().and_then(|store| store.active().ok());
            let lora = lora_state(&config, store.as_ref(), active.as_ref());
            (active.map(|version| version.id), label, lora)
        });
    }
}

/// LoRA status of the main model. Endpoint models can never take an adapter. Otherwise the capability the
/// learning loop recorded wins over the static architecture check. Admission alone never proves runtime use.
pub(super) fn lora_state(
    config: &Config,
    store: Option<&Store>,
    active: Option<&crate::config::types::Version>,
) -> Lora {
    if !config.runs_dial_mlx() {
        return Lora::Unsupported;
    }
    if let Some(phase) = store.and_then(|store| crate::learn::improve::lora_phase(config, store)) {
        match phase.as_str() {
            "training" => return Lora::Training,
            "paused" => return Lora::Paused,
            "evaluating" => return Lora::Evaluating,
            _ => {}
        }
    }
    if let Some(version) = active.filter(|version| version.adapter.is_some()) {
        return Lora::Active(version.id);
    }
    let recorded = store
        .and_then(|store| store.get_meta("serving_capabilities").ok().flatten())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(crate::learn::worker::current_lora_probe)
        .and_then(|caps| caps["lora"].as_bool());
    let compatible = recorded
        .or_else(|| catalog::local_lora_support(&config.user_dir, Path::new(&config.model)));
    match compatible {
        Some(false) => Lora::Unsupported,
        None => Lora::Unknown,
        Some(true) if recorded != Some(true) => Lora::Unverified,
        Some(true) if !config.training.enabled || !config.improve.enabled => Lora::Off,
        Some(true) => match active.filter(|version| version.adapter.is_some()) {
            Some(version) => Lora::Active(version.id),
            None => Lora::Ready,
        },
    }
}

fn improvement_label(config: &Config, store: Option<&Store>) -> String {
    if !config.improve.enabled {
        return "learning off".into();
    }
    if store.is_some_and(|s| !auto::enabled(config, s).unwrap_or(false)) {
        return "learning paused".into();
    }
    let state = store
        .and_then(|store| store.get_meta("auto_status").ok().flatten())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match state.as_deref() {
        Some("queued") => "improvement queued".into(),
        Some("running") => store
            .and_then(|store| store.get_meta("cycle_progress").ok().flatten())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| {
                value["stage"]
                    .as_str()
                    .map(|stage| format!("improving: {}", agent::truncate(stage, 80)))
            })
            .unwrap_or_else(|| "improving".into()),
        Some("failed") => "learning failed".into(),
        Some("disabled") => "learning paused".into(),
        Some(_) => store
            .and_then(|store| auto::readiness(config, store).ok())
            .unwrap_or_else(|| "learning idle".into()),
        None if store.is_none() => "learning unavailable".into(),
        None => "learning idle".into(),
    }
}

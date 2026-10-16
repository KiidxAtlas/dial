//! Memory backend readiness shared by `/setup memory` and `dial doctor`: what each backend needs
//! before it works and whether the current settings already provide it. Pure functions of
//! [`Config`]; nothing here reads or writes files.

use crate::config::Config;
use crate::storage::memory_backend::config::{
    EmbeddingMode, MemoryBackendId, auto_retain_uses_memory_role, half_embedding_override,
};

/// Memory backends in settings order: id, name, and what it needs before it works.
pub const BACKENDS: &[(MemoryBackendId, &str, &str)] = &[
    (
        MemoryBackendId::Off,
        "Off",
        "Nothing is remembered between sessions.",
    ),
    (
        MemoryBackendId::Provenance,
        "Provenance facts",
        "No setup: facts with history and expiry, kept on this machine.",
    ),
    (
        MemoryBackendId::Semantic,
        "Semantic facts",
        "No required setup. Optional: an embedding model for recall by meaning, and the memory role to save facts automatically.",
    ),
    (
        MemoryBackendId::Local,
        "Rollout summary",
        "Needs the memory role (Alt+M): a model summarises past sessions once they have been idle.",
    ),
    (
        MemoryBackendId::Sharpshooter,
        "Project decisions",
        "Needs the memory role (Alt+M) or a model override: a model keeps project decision notes.",
    ),
    (
        MemoryBackendId::Hindsight,
        "Hindsight service",
        "Needs a Hindsight service you run: its URL, and an API key variable when it is remote.",
    ),
];

pub fn backend_label(id: MemoryBackendId) -> &'static str {
    BACKENDS
        .iter()
        .find(|(backend, _, _)| *backend == id)
        .map_or("Unknown", |(_, label, _)| label)
}

pub fn parse_backend(name: &str) -> Option<MemoryBackendId> {
    BACKENDS
        .iter()
        .map(|(id, _, _)| *id)
        .find(|id| id.as_str() == name)
}

/// The backend the settings select, or `None` when memory is off.
pub fn selected(config: &Config) -> Option<MemoryBackendId> {
    config.memory.enabled.then_some(config.memory.backend)
}

/// What `backend` still lacks with this config: `(blocking, optional)` next steps. Blocking items
/// leave the backend inactive; optional ones only limit what it does.
pub fn backend_missing(config: &Config, backend: MemoryBackendId) -> (Vec<String>, Vec<String>) {
    let mut probe = config.clone();
    probe.memory.backend = backend;
    probe.memory.enabled = true;
    let (mut blocking, mut optional) = (Vec::new(), Vec::new());
    match backend {
        MemoryBackendId::Local | MemoryBackendId::Sharpshooter => {
            if let Some(error) = probe.memory_setup_error() {
                blocking.push(format!("Needs a model: {error}."));
            }
        }
        MemoryBackendId::Hindsight => {
            if probe.memory.hindsight.endpoint.trim().is_empty() {
                blocking.push(
                    "Set the Hindsight Service URL in /settings → Memory (choose Hindsight service there)."
                        .to_owned(),
                );
            } else if let Err(error) = probe.validate() {
                blocking.push(format!(
                    "Fix the Hindsight settings in /settings → Memory: {error:#}."
                ));
            }
        }
        MemoryBackendId::Semantic => {
            let semantic = &probe.memory.semantic;
            let override_set = !semantic.embedding_base_url.trim().is_empty()
                && !semantic.embedding_model.trim().is_empty();
            if let Some(half) = half_embedding_override(semantic) {
                blocking.push(format!("{half}."));
            } else if semantic.embedding == EmbeddingMode::On
                && !override_set
                && probe.role_selection("embedding").is_none()
            {
                blocking.push("Embeddings are on but no embedding model is set: assign the embedding role (Alt+M) or run /setup embeddings.".to_owned());
            } else if semantic.embedding != EmbeddingMode::Off
                && !override_set
                && probe.role_selection("embedding").is_none()
            {
                optional.push("Recall is keyword-only until you assign the embedding role (Alt+M) or run /setup embeddings.".to_owned());
            }
            if auto_retain_uses_memory_role(&probe.memory)
                && probe.role_selection("memory").is_none()
            {
                optional.push(
                    "Assign the memory role (Alt+M) to save facts automatically; until then only the retain tool saves them."
                        .to_owned(),
                );
            }
        }
        MemoryBackendId::Off | MemoryBackendId::Provenance => {}
    }
    (blocking, optional)
}

/// One-line readiness from [`backend_missing`]'s result.
pub fn status_summary(blocking: &[String], optional: &[String]) -> String {
    match (blocking.first(), optional.is_empty()) {
        (Some(first), _) => format!("Not ready: {first}"),
        (None, true) => "Ready".to_owned(),
        (None, false) => "Ready (optional steps left)".to_owned(),
    }
}

/// The memory section of `dial doctor`: the selected backend with what it still needs, then every
/// backend with its readiness and the command that selects it.
pub fn doctor_text(config: &Config) -> String {
    let current = selected(config);
    let mut lines = Vec::new();
    match current {
        None => lines.push("Memory: off (nothing is remembered between sessions)".to_owned()),
        Some(id) => {
            let (blocking, optional) = backend_missing(config, id);
            lines.push(format!(
                "Memory: {} — {}",
                backend_label(id),
                status_summary(&blocking, &optional)
            ));
            lines.extend(
                blocking
                    .into_iter()
                    .map(|line| format!("  Still needed: {line}")),
            );
            lines.extend(
                optional
                    .into_iter()
                    .map(|line| format!("  Optional: {line}")),
            );
        }
    }
    lines.push(
        "Backends (switch with `dial config set memory.backend <id>` or /setup memory):".to_owned(),
    );
    let width = BACKENDS
        .iter()
        .map(|(id, _, _)| id.as_str().len())
        .max()
        .unwrap_or(0);
    for (id, label, _) in BACKENDS {
        let (blocking, optional) = backend_missing(config, *id);
        lines.push(format!(
            "  {:<width$}  {label}{} · {}",
            id.as_str(),
            if current == Some(*id) {
                " (current)"
            } else {
                ""
            },
            status_summary(&blocking, &optional),
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_text_marks_the_current_backend_and_lists_every_backend() {
        let config = Config::default();
        let text = doctor_text(&config);
        assert!(
            text.starts_with("Memory: Provenance facts — Ready"),
            "{text}"
        );
        for (id, _, _) in BACKENDS {
            assert!(text.contains(&format!("  {}", id.as_str())), "{text}");
        }
        assert!(text.contains("(current)"), "{text}");
        let local = text
            .lines()
            .find(|l| l.trim_start().starts_with("local"))
            .unwrap();
        assert!(local.contains("Not ready"), "{local}");
    }

    #[test]
    fn doctor_text_reports_off_and_unmet_steps_of_the_selected_backend() {
        let mut config = Config::default();
        config.memory.enabled = false;
        assert!(doctor_text(&config).starts_with("Memory: off"));
        config.memory.enabled = true;
        config.memory.backend = MemoryBackendId::Semantic;
        let text = doctor_text(&config);
        assert!(
            text.starts_with("Memory: Semantic facts — Ready (optional steps left)"),
            "{text}"
        );
        assert!(
            text.contains("  Optional: Recall is keyword-only"),
            "{text}"
        );
    }
}

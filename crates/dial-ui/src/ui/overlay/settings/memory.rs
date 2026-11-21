//! Memory backend rows: the summary, the role gaps and the loopback check that drive them.
use super::SettingsOverlay;
use crate::config::schema::Kind;
use toml::Value;

/// Plain-language summary of a memory backend. `models` is the chord that opens the model selector
/// (its Roles view assigns the memory role), so the hint follows the user's keymap.
pub(super) fn memory_summary(backend: &str, models: &str) -> String {
    match backend {
        "off" => {
            "Memory is off: nothing carries over between sessions and the memory tools are hidden."
                .to_owned()
        }
        "provenance" => {
            "Saves facts the agent learns, with history and expiry, found again by keyword. Needs no setup."
                .to_owned()
        }
        "local" => format!(
            "Summarises your past sessions into project memory, using the model on the Memory role ({models}, Roles)."
        ),
        "hindsight" => {
            "Connects to a Hindsight memory service you run. Needs its URL, and an API key variable for a remote server."
                .to_owned()
        }
        "sharpshooter" => format!(
            "Keeps project decision notes, written by the model on the Memory role ({models}, Roles)."
        ),
        "semantic" => format!(
            "Stores facts on this machine and recalls them by keyword, and by meaning once an embedding model is assigned (Embedding model below, or {models}, Roles). Facts are saved automatically only after you assign the memory role ({models}); that model receives turn text."
        ),
        _ => String::new(),
    }
}

/// Settings prefix holding one backend's own options.
pub(super) fn memory_section(backend: &str) -> Option<&'static str> {
    match backend {
        "local" => Some("memory.local."),
        "hindsight" => Some("memory.hindsight."),
        "sharpshooter" => Some("memory.sharpshooter."),
        "semantic" => Some("memory.semantic."),
        _ => None,
    }
}

/// A model role the backend cannot work without and that only the Roles view can assign.
pub(super) fn memory_missing_role(
    backend: &str,
    get: impl Fn(&str) -> Value,
    has_role: impl Fn(&str) -> bool,
) -> Option<String> {
    let fields: &[&str] = match backend {
        "local" => &[
            "memory.local.extraction_role",
            "memory.local.consolidation_role",
        ],
        _ => &[],
    };
    fields
        .iter()
        .filter_map(|path| get(path).as_str().map(str::to_owned))
        .find(|role| !has_role(role))
}

/// The display name of a `memory.backend` choice (`Hindsight service`).
pub(super) fn label_for_choice(backend: &str) -> &'static str {
    crate::config::schema::registry()
        .iter()
        .find(|d| d.path == "memory.backend")
        .and_then(|d| match d.kind {
            Kind::Enum(options) => options.iter().find(|(v, _)| *v == backend).map(|(_, l)| *l),
            _ => None,
        })
        .unwrap_or("That backend")
}

impl SettingsOverlay {
    pub(super) fn memory_backend(&self) -> String {
        self.current_path("memory.backend")
            .as_str()
            .unwrap_or("provenance")
            .to_owned()
    }

    /// Memory rows that do nothing for the selected backend stay out of the way.
    pub(super) fn memory_row_shown(&self, path: &str) -> bool {
        if !path.starts_with("memory.") || path == "memory.backend" {
            return true;
        }
        let backend = self.memory_backend();
        if backend == "off" {
            return false;
        }
        let own = [Some(backend.as_str()), self.memory_intent.as_deref()];
        ["local", "hindsight", "sharpshooter", "semantic"]
            .iter()
            .filter_map(|b| memory_section(b).map(|prefix| (b, prefix)))
            .all(|(b, prefix)| !path.starts_with(prefix) || own.contains(&Some(*b)))
    }
}

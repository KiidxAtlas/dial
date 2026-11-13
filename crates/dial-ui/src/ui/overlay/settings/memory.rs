//! Memory backend rows: the summary, the role gaps and the loopback check that drive them.
use super::{EMBEDDING_MODEL_PATH, EMBEDDING_URL_PATH, SettingsOverlay};
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

/// Whether `endpoint` names this machine, which may use plain HTTP and needs no API key.
/// Mirrors the loopback rule in `memory_backend::config::endpoint`.
pub(super) fn is_loopback_url(endpoint: &str) -> bool {
    url::Url::parse(endpoint).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            host == "localhost"
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    })
}

/// Required fields of `backend` that are still empty or half set, as setting paths, in the order to
/// fill them. Mirrors `memory_backend::config::validate` and the runtime routes: Hindsight's URL and
/// key, Sharpshooter's model when the memory role is unassigned, and for semantic the missing half of
/// the URL/model override, or an embedding model when embeddings are `on` with no route.
pub(super) fn memory_needs(
    backend: &str,
    get: impl Fn(&str) -> Value,
    has_role: impl Fn(&str) -> bool,
) -> Vec<&'static str> {
    let text = |path: &str| get(path).as_str().unwrap_or("").trim().to_owned();
    let mut needs = Vec::new();
    match backend {
        "hindsight" => {
            let endpoint = text("memory.hindsight.endpoint");
            if endpoint.is_empty() {
                needs.push("memory.hindsight.endpoint");
            } else if text("memory.hindsight.api_key_env").is_empty() && !is_loopback_url(&endpoint)
            {
                needs.push("memory.hindsight.api_key_env");
            }
        }
        "sharpshooter" => {
            if text("memory.sharpshooter.model").is_empty()
                && !has_role(&text("memory.sharpshooter.model_role"))
            {
                needs.push("memory.sharpshooter.model");
            }
        }
        "semantic" => {
            let (url, model) = (text(EMBEDDING_URL_PATH), text(EMBEDDING_MODEL_PATH));
            match (url.is_empty(), model.is_empty()) {
                (false, true) => needs.push(EMBEDDING_MODEL_PATH),
                (true, false) => needs.push(EMBEDDING_URL_PATH),
                (true, true)
                    if text("memory.semantic.embedding") == "on" && !has_role("embedding") =>
                {
                    needs.push(EMBEDDING_MODEL_PATH)
                }
                _ => {}
            }
        }
        _ => {}
    }
    needs
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

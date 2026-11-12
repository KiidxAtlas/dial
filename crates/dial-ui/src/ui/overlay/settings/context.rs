//! Context/history presentation uses the same settings rows and recorded session facts.
use super::*;

/// Cells in the context gauge.
const GAUGE_CELLS: usize = 16;

/// Compaction settings the context page keeps in view; see `context_advanced`.
const CONTEXT_BASIC: &[&str] = &[
    "compaction.enabled",
    "compaction.auto_continue",
    "compaction.reserve_tokens",
    "compaction.keep_recent_tokens",
];

impl SettingsOverlay {
    pub(super) fn context_setting(def: &SettingDef) -> bool {
        def.path.starts_with("compaction.")
            || matches!(
                def.path,
                "context_window"
                    | "cloud_context_window"
                    | "local_context_window"
                    | "harness.max_tokens"
                    | "efficiency.history_token_cap"
            )
    }

    /// The context page's summary card: a gauge of the active request's use of its window and
    /// where compaction starts, then the read-only server facts. Unknown facts say so.
    pub(super) fn context_summary(&self, ctx: &DrawCtx) -> Vec<String> {
        if !self.context_only {
            return self.memory_landing();
        }
        let status = ctx.status;
        let ascii = ctx.theme.ascii();
        let count = |n: u64| text::compact_count(n);
        let window = (status.context_window > 0).then_some(status.context_window as u64);
        let used = status.prompt_tokens;
        let estimated = if status.context_estimated {
            ", estimated"
        } else {
            ""
        };
        let mut gauge = String::from("Context & history · ");
        match (used, window) {
            (Some(used), Some(window)) => {
                let share = (used as f64 / window as f64).clamp(0.0, 1.0);
                let filled = (share * GAUGE_CELLS as f64).round() as usize;
                let (full, empty) = if ascii { ("#", ".") } else { ("█", "░") };
                gauge.push_str(&format!(
                    "{}{} {} of {} used ({:.0}%{estimated})",
                    full.repeat(filled),
                    empty.repeat(GAUGE_CELLS - filled),
                    count(used),
                    count(window),
                    share * 100.0
                ));
            }
            // Each missing fact says why it is missing or how to supply it.
            (None, Some(window)) => {
                gauge.push_str(&format!("{} window · nothing sent yet", count(window)));
            }
            (Some(used), None) => {
                gauge.push_str(&format!(
                    "{} used{estimated} · window not known: set Current model context below",
                    count(used)
                ));
            }
            (None, None) => {
                gauge.push_str(
                    "nothing sent yet · window not known: set Current model context below",
                );
            }
        }
        if let (Some(window), Some(reserve)) = (window, status.compaction_reserve) {
            let at = window.saturating_sub(reserve as u64);
            gauge.push_str(&format!(
                " · compacts near {} (~{:.0}%)",
                count(at),
                at as f64 * 100.0 / window as f64
            ));
        }
        let loaded = status.loaded_context_window.map_or_else(
            || "not reported by the server".to_owned(),
            |n| format!("{} (read-only)", count(n as u64)),
        );
        let reserve = status
            .compaction_reserve
            .map_or_else(|| "not set".to_owned(), |n| count(n as u64));
        vec![
            gauge,
            format!("Server capacity {loaded} · compaction reserve {reserve}"),
            "Values below are saved defaults; the active request follows reported capacity."
                .to_owned(),
        ]
    }

    /// Context settings shown without opening "Show advanced": the window sizes, output cap and
    /// whether and how compaction runs. The other compaction tuning folds away.
    pub(super) fn context_advanced(&self, path: &str) -> bool {
        self.context_only && path.starts_with("compaction.") && !CONTEXT_BASIC.contains(&path)
    }

    fn memory_landing(&self) -> Vec<String> {
        if !self.search.is_empty()
            || self.changed_only
            || !self
                .tabs
                .get(self.tab)
                .is_some_and(|(tab, _)| *tab == "Memory")
        {
            return Vec::new();
        }
        let backend = self.memory_backend();
        let model = |role: &str| {
            crate::config::schema::get(&self.effective, &format!("model_roles.{role}.model"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map_or_else(|| "unassigned".to_owned(), text::one_line)
        };
        let override_model = self.current_path(EMBEDDING_MODEL_PATH);
        let embedding = override_model
            .as_str()
            .filter(|value| !value.is_empty())
            .map_or_else(|| model("embedding"), text::one_line);
        let scope = match backend.as_str() {
            "provenance" | "semantic" => "project facts + explicit user-wide facts",
            "local" | "sharpshooter" => "project",
            "hindsight" => "configured service bank",
            "off" => "disabled",
            _ => "unknown",
        };
        let readiness = self
            .hints
            .get("memory.backend")
            .or_else(|| self.hints.get("memory.enabled"))
            .map(String::as_str)
            .unwrap_or("Unknown · prerequisite evidence unavailable");
        vec![
            format!("Memory · backend: {backend} · storage scope: {scope}"),
            format!("Extraction model: {} · embedding model: {embedding}", model("memory")),
            format!("Readiness: {readiness}"),
            "Backend controls enablement; choosing a model alone never enables memory. Model assignment: /roles; prerequisites: /setup memory.".to_owned(),
        ]
    }
}

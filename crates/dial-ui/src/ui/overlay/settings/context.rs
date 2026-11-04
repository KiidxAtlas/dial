//! Context/history presentation uses the same settings rows and recorded session facts.
use super::*;

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

    pub(super) fn context_summary(&self, ctx: &DrawCtx) -> Vec<String> {
        if !self.context_only {
            return self.memory_landing();
        }
        let status = ctx.status;
        let active = if status.context_window == 0 {
            "unknown".to_owned()
        } else {
            status.context_window.to_string()
        };
        let loaded = status
            .loaded_context_window
            .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
        let used = status.prompt_tokens.map_or_else(
            || "unknown".to_owned(),
            |n| {
                format!(
                    "{n}{}",
                    if status.context_estimated {
                        " estimated"
                    } else {
                        ""
                    }
                )
            },
        );
        let reserve = status
            .compaction_reserve
            .map_or_else(|| "unknown".to_owned(), |n| n.to_string());
        vec![
            format!("Context & history · active limit {active} · used {used}"),
            format!("Loaded server capacity: {loaded} (read-only) · compaction reserve: {reserve}"),
            "Saved caps/output/compaction below are defaults, not a claim about the active request. Auto follows reported capacity; loaded server limits still apply.".to_owned(),
        ]
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

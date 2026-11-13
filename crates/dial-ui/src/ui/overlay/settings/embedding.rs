//! The embedding model picker: opening it on the memory row and applying the choice.
use super::super::OverlayEvent;
use super::format::setting_edit_text;
use super::{EMBEDDING_URL_PATH, Mode, Pending, SettingsOverlay};
use crate::config::schema::SettingDef;
use crate::ui::editor::Editor;
use toml::Value;

impl SettingsOverlay {
    /// The Embedding model row's value: the override when set, otherwise the embedding role.
    pub(super) fn embedding_row_text(&self, value: &Value) -> String {
        let model = value.as_str().unwrap_or("").trim();
        if !model.is_empty() {
            return format!("{model} · URL override");
        }
        match self.embedding_role() {
            Some(name) => format!("{name} · embedding role"),
            None => "not set · Enter to choose".to_owned(),
        }
    }

    /// `provider/model` of the assigned embedding role.
    pub(super) fn embedding_role(&self) -> Option<String> {
        let role = crate::config::schema::get(&self.effective, "model_roles.embedding")?;
        let field = |key: &str| role.get(key).and_then(Value::as_str).unwrap_or("");
        Some(format!("{}/{}", field("provider"), field("model")))
    }

    /// Enter on the Embedding model row: discovered models for the embedding role, curated models to
    /// set up, then the advanced override.
    pub(super) fn open_embedding_picker(&mut self, def: &'static SettingDef) -> OverlayEvent {
        let assigned = self.embedding_role();
        let mut choices: Vec<String> = (0..self.embedding_models.len())
            .map(|i| format!("role:{i}"))
            .collect();
        let discovered = |name: &str| {
            self.embedding_models.iter().any(|model| {
                model.selection.provider == "ollama"
                    && model.selection.model.trim_end_matches(":latest") == name
            })
        };
        choices.extend(
            crate::setup::embeddings::CATALOG
                .iter()
                .filter(|model| !discovered(model.name))
                .map(|model| format!("setup:{}", model.name)),
        );
        choices.push("override".to_owned());
        let has_override = self
            .current(def)
            .as_str()
            .is_some_and(|model| !model.trim().is_empty());
        if has_override {
            choices.push("clear".to_owned());
        }
        // A URL override waiting for its model lands on typing the model name.
        let url_set = self
            .current_path(EMBEDDING_URL_PATH)
            .as_str()
            .is_some_and(|url| !url.trim().is_empty());
        let assigned_choice = |choice: &String| {
            choice
                .strip_prefix("role:")
                .and_then(|i| i.parse::<usize>().ok())
                .and_then(|i| self.embedding_models.get(i))
                .is_some_and(|model| {
                    assigned.as_deref()
                        == Some(&format!(
                            "{}/{}",
                            model.selection.provider, model.selection.model
                        ))
                })
        };
        let cursor = choices
            .iter()
            .position(|choice| {
                if url_set {
                    choice == "override"
                } else {
                    assigned_choice(choice)
                }
            })
            .unwrap_or(0);
        self.notes.remove(def.path);
        self.mode = Mode::Pick {
            def,
            choices,
            cursor,
            filter: Editor::default(),
        };
        OverlayEvent::None
    }

    pub(super) fn embedding_choice_label(&self, choice: &str) -> String {
        if let Some(model) = choice
            .strip_prefix("role:")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.embedding_models.get(i))
        {
            let name = format!("{}/{}", model.selection.provider, model.label);
            return if model.classified {
                format!("{name} · embedding role")
            } else {
                format!(
                    "{name} · {}",
                    crate::ui::overlay::models::UNCLASSIFIED_LABEL
                )
            };
        }
        if let Some(name) = choice.strip_prefix("setup:") {
            let size = crate::setup::embeddings::find(name)
                .map(|model| format!(", {} MB", model.size_mb))
                .unwrap_or_default();
            return format!("{name} · set up with Ollama{size}, then Ctrl+L");
        }
        match choice {
            "override" => {
                "Type a model name (advanced override; needs Embedding service URL)".into()
            }
            "clear" => "Clear the URL/model override (use the embedding role)".into(),
            other => other.to_owned(),
        }
    }

    /// A choice on the Embedding model row: assign the role, arm a curated setup for Ctrl+L, edit
    /// the override, or clear it.
    pub(super) fn embedding_pick(
        &mut self,
        def: &'static SettingDef,
        choice: &str,
    ) -> OverlayEvent {
        if let Some(model) = choice
            .strip_prefix("role:")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| self.embedding_models.get(i))
        {
            let overridden = self
                .current(def)
                .as_str()
                .is_some_and(|model| !model.trim().is_empty());
            self.feedback = overridden.then(|| {
                "The URL/model override still wins over the embedding role; clear it to use the role"
                    .to_owned()
            });
            return OverlayEvent::AssignModel {
                role: "embedding".into(),
                selection: model.selection.clone(),
                scope: self.scope,
            };
        }
        if let Some(name) = choice.strip_prefix("setup:") {
            match self.embedding_setups.get(name).cloned() {
                Some(offer) => {
                    self.setup_armed = Some(offer.id.clone());
                    self.feedback = Some(format!(
                        "Review the plan below, then press Ctrl+L to set up {}",
                        offer.title
                    ));
                    self.setup_offers.insert(def.path.to_owned(), offer);
                }
                None => {
                    self.feedback = Some(format!("Run /setup embeddings:{name} to set it up"));
                }
            }
            return OverlayEvent::None;
        }
        match choice {
            "override" => {
                let mut editor = Editor::default();
                editor.set(&setting_edit_text(def, &self.edit_seed(def)));
                self.reveal_sensitive = false;
                self.mode = Mode::Edit { def, editor };
                OverlayEvent::None
            }
            "clear" => {
                self.notes.remove(def.path);
                self.pending
                    .insert(def.path.to_owned(), Pending::Unset(self.scope));
                OverlayEvent::Unset {
                    path: def.path.to_owned(),
                    scope: self.scope,
                }
            }
            _ => OverlayEvent::None,
        }
    }
}

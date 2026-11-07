//! Offline, draft-first configuration of the existing provider fallback engine.
use crate::config::settings_store::{Change, Scope};
use crate::config::types::{Config, FallbackOn, FallbackRevert};
use crate::llm::{fallback, model_config::ModelSelection};
use crate::ui::{
    editor::Editor,
    keys::Keymap,
    overlay::{DrawCtx, OverlayEvent},
    theme::Tok,
};
use anyhow::{Result, ensure};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::{
    Frame,
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};

pub(in crate::app) struct BackupOverlay {
    base: Config,
    key: Editor,
    chain: Editor,
    row: usize,
    choice: usize,
    choices: Vec<String>,
    filter: Editor,
    backup_index: usize,
    scope: Scope,
    on: FallbackOn,
    revert: FallbackRevert,
    reviewing: bool,
    notice: String,
    project: toml::Table,
}

impl BackupOverlay {
    pub(in crate::app) fn new(config: &Config, scope: Scope) -> Self {
        let mut key = Editor::default();
        key.set("default");
        let mut chain = Editor::default();
        chain.set(
            &config
                .retry
                .fallback_chains
                .get("default")
                .cloned()
                .unwrap_or_default()
                .join(", "),
        );
        let mut choices: Vec<String> = config.model_favorites.iter().map(qualified).collect();
        choices.extend(
            config
                .model_profiles
                .values()
                .map(|p| format!("{}/{}", p.provider, p.model)),
        );
        choices.extend(
            crate::llm::model_config::ROLES
                .iter()
                .filter(|(role, _)| !matches!(*role, "main" | "embedding" | "image"))
                .map(|(role, _)| format!("@{role}")),
        );
        choices.sort();
        choices.dedup();
        Self {
            base: config.clone(),
            key,
            chain,
            row: 0,
            choice: 0,
            filter: Editor::default(),
            backup_index: 0,
            choices,
            scope,
            on: config.retry.fallback_on,
            revert: config.retry.fallback_revert,
            reviewing: false,
            notice: "Draft only · no provider requests, discovery or downloads".into(),
            project: toml::Table::new(),
        }
    }

    /// Only already-cached chat choices are admitted; this never starts catalog discovery.
    pub(in crate::app) fn set_choices(&mut self, discovery: &crate::llm::catalog::Discovery) {
        self.choices.extend(
            discovery
                .models
                .iter()
                .filter(|entry| entry.supports_chat())
                .map(|entry| qualified(&entry.selection)),
        );
        self.choices.sort();
        self.choices.dedup();
        self.choice = self.choice.min(self.choices.len().saturating_sub(1));
    }

    pub(in crate::app) fn set_project_layer(&mut self, project: toml::Table) {
        self.project = project;
    }

    fn selectors(&self) -> Vec<String> {
        self.chain
            .text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn offered(&self) -> Vec<String> {
        let filter = self.filter.text.to_lowercase();
        self.choices
            .iter()
            .filter(|choice| choice.to_lowercase().contains(&filter))
            .cloned()
            .collect()
    }

    fn candidate(&self) -> Result<Config> {
        let key = self.key.text.trim();
        ensure!(
            key == "default"
                || crate::llm::model_config::ROLES
                    .iter()
                    .any(|(role, _)| *role == key && *role != "main")
                || key
                    .split_once('/')
                    .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty()),
            "Choose default, a role, provider/model or provider/*"
        );
        let mut config = self.base.clone();
        let selectors = self.selectors();
        for selector in &selectors {
            if let Some(role) = selector.strip_prefix('@') {
                ensure!(
                    crate::llm::model_config::ROLES
                        .iter()
                        .any(|(id, _)| *id == role),
                    "Unknown role selector {selector}"
                );
            }
        }
        config
            .retry
            .fallback_chains
            .insert(key.to_owned(), selectors);
        config.retry.fallback_on = self.on;
        config.retry.fallback_revert = self.revert;
        if self.scope == Scope::Global {
            if let Some(toml::Value::Table(chains)) =
                crate::config::schema::get(&self.project, "retry.fallback_chains")
            {
                for (key, value) in chains {
                    let selectors: Vec<String> = value.clone().try_into()?;
                    config.retry.fallback_chains.insert(key.clone(), selectors);
                }
            }
            if let Some(value) = crate::config::schema::get(&self.project, "retry.fallback_on") {
                config.retry.fallback_on = value.clone().try_into()?;
            }
            if let Some(value) = crate::config::schema::get(&self.project, "retry.fallback_revert")
            {
                config.retry.fallback_revert = value.clone().try_into()?;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub(super) fn changes(&self, layer: &toml::Table) -> Result<Vec<Change>> {
        ensure!(self.reviewing, "Review the backup policy before saving");
        self.candidate()?;
        let mut chains = crate::config::schema::get(layer, "retry.fallback_chains")
            .and_then(toml::Value::as_table)
            .cloned()
            .unwrap_or_default();
        chains.insert(
            self.key.text.trim().to_owned(),
            toml::Value::try_from(self.selectors())?,
        );
        Ok(vec![
            Change::Set {
                path: "retry.fallback_chains".into(),
                value: toml::Value::Table(chains),
            },
            Change::Set {
                path: "retry.fallback_on".into(),
                value: toml::Value::try_from(self.on)?,
            },
            Change::Set {
                path: "retry.fallback_revert".into(),
                value: toml::Value::try_from(self.revert)?,
            },
        ])
    }

    pub(super) fn scope(&self) -> Scope {
        self.scope
    }

    pub(super) fn check_unchanged(&self, current: &Config) -> Result<()> {
        ensure!(
            current.settings_values()? == self.base.settings_values()?,
            "Settings changed since this draft opened; close and reopen Backup models to review the new routes"
        );
        Ok(())
    }

    pub(super) fn saved(&mut self, config: Config) {
        self.base = config;
        self.reviewing = false;
        self.notice =
            "Saved · adopted through normal settings boundaries; an in-flight request is unchanged"
                .into();
    }

    pub(super) fn banner(&mut self, message: String, _: bool) {
        self.notice = message;
    }

    pub(super) fn handle(&mut self, event: &Event, keys: &Keymap) -> OverlayEvent {
        if let Event::Paste(text) = event {
            if !self.reviewing && self.row < 2 {
                self.editor().insert(text);
            }
            return OverlayEvent::None;
        }
        let Event::Key(key) = event else {
            return OverlayEvent::None;
        };
        if self.reviewing {
            return match key.code {
                KeyCode::Esc => {
                    self.reviewing = false;
                    OverlayEvent::None
                }
                KeyCode::Char('y') if key.modifiers.is_empty() => {
                    OverlayEvent::Action("backup-models:save".into())
                }
                _ => OverlayEvent::None,
            };
        }
        if self.row == 1
            && matches!(key.code, KeyCode::Left | KeyCode::Right | KeyCode::Delete)
            && (key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::CONTROL))
        {
            let mut items = self.selectors();
            if !items.is_empty() {
                self.backup_index = self.backup_index.min(items.len() - 1);
                let next = if key.code == KeyCode::Left {
                    self.backup_index.saturating_sub(1)
                } else {
                    (self.backup_index + 1).min(items.len() - 1)
                };
                if key.code == KeyCode::Delete {
                    items.remove(self.backup_index);
                } else if key.modifiers.contains(KeyModifiers::ALT) {
                    items.swap(self.backup_index, next);
                    self.backup_index = next;
                } else {
                    self.backup_index = next;
                }
                self.chain.set(&items.join(", "));
                self.notice = format!(
                    "Selected backup #{} · Ctrl+←/→ select · Alt+←/→ reorder · Ctrl+Delete remove",
                    self.backup_index + 1
                );
            }
            return OverlayEvent::None;
        }
        match key.code {
            KeyCode::Enter if self.row == 0 => {
                self.chain.set(
                    &self
                        .base
                        .retry
                        .fallback_chains
                        .get(self.key.text.trim())
                        .cloned()
                        .unwrap_or_default()
                        .join(", "),
                );
                self.row = 1;
            }
            KeyCode::Esc => return OverlayEvent::Close,
            KeyCode::Tab | KeyCode::Down => self.row = (self.row + 1) % 7,
            KeyCode::BackTab | KeyCode::Up => self.row = (self.row + 6) % 7,
            KeyCode::Enter if self.row == 6 => match self.candidate() {
                Ok(_) => self.reviewing = true,
                Err(error) => self.notice = format!("{error:#}"),
            },
            KeyCode::Enter if self.row == 2 => {
                if let Some(choice) = self.offered().get(self.choice) {
                    let mut items = self.selectors();
                    items.push(choice.clone());
                    self.chain.set(&items.join(", "));
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Enter if self.row >= 2 => match self.row {
                2 if !self.offered().is_empty() => {
                    let len = self.offered().len();
                    self.choice = (self.choice
                        + if key.code == KeyCode::Left {
                            len - 1
                        } else {
                            1
                        })
                        % len;
                }
                3 => {
                    self.on = match self.on {
                        FallbackOn::Any => FallbackOn::UsageLimit,
                        FallbackOn::UsageLimit => FallbackOn::ExceptUsageLimit,
                        FallbackOn::ExceptUsageLimit => FallbackOn::Any,
                    }
                }
                4 => {
                    self.revert = match self.revert {
                        FallbackRevert::CooldownExpiry => FallbackRevert::Never,
                        FallbackRevert::Never => FallbackRevert::CooldownExpiry,
                    }
                }
                5 => {
                    self.scope = match self.scope {
                        Scope::Global => Scope::Project,
                        Scope::Project => Scope::Global,
                    }
                }
                _ => {}
            },
            _ if self.row == 2 => {
                self.filter.key_with(*key, keys);
                self.choice = 0;
            }
            _ if self.row < 2 && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.editor().key_with(*key, keys);
            }
            _ => {}
        }
        OverlayEvent::None
    }

    fn editor(&mut self) -> &mut Editor {
        if self.row == 0 {
            &mut self.key
        } else {
            &mut self.chain
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, ctx: &DrawCtx) {
        let mut lines = vec![
            Line::from("Configure explicit backups; nothing runs while browsing or saving."),
            Line::default(),
        ];
        let values = [
            format!("Chain key: {}", self.key.text),
            format!(
                "Backups in order (selected #{}): {}",
                self.backup_index + 1,
                self.chain.text
            ),
            format!(
                "Add model · filter [{}]: {}",
                self.filter.text,
                self.offered()
                    .get(self.choice)
                    .map_or("No matching choices", String::as_str)
            ),
            format!("Failure trigger: {:?}", self.on),
            format!("Return: {:?}", self.revert),
            format!("Save scope: {:?}", self.scope),
            "Review changes →".into(),
        ];
        for (row, value) in values.into_iter().enumerate() {
            lines.push(Line::styled(
                format!("{} {value}", if row == self.row { ">" } else { " " }),
                ctx.theme.fg(if row == self.row {
                    Tok::Accent
                } else {
                    Tok::Text
                }),
            ));
        }
        lines.extend([
            Line::default(),
            Line::from("Tab/↑/↓ rows · ←/→ choices · Enter adds/reviews · Esc cancels"),
            Line::from(
                "Edit the ordered list to reorder/remove; empty list explicitly disables this key.",
            ),
            Line::from("On backups: Ctrl+←/→ select · Alt+←/→ reorder · Ctrl+Delete remove"),
            Line::from(
                "Cooldown: server retry-after, otherwise 5 minutes. Never = manual /model return.",
            ),
            Line::default(),
        ]);
        match self.candidate() {
            Ok(config) => {
                let role = if self.key.text.contains('/') {
                    "default"
                } else {
                    self.key.text.trim()
                };
                let route = if self.key.text.contains('/') && !self.key.text.ends_with("/*") {
                    fallback::resolve_selector(&config, &config.selection(), self.key.text.trim())
                        .map_or_else(
                            || config.clone(),
                            |selected| config.with_selection(&selected),
                        )
                } else if role == "default" {
                    config.clone()
                } else {
                    config.for_role(role)
                };
                let selected = route.selection();
                let winner =
                    fallback::winning_chain_key(&config.retry.fallback_chains, role, &selected);
                let chain = fallback::fallback_models(&route, role, &selected);
                lines.push(Line::from(format!(
                    "Preview role {role}: {}",
                    qualified(&selected)
                )));
                lines.push(Line::from(format!(
                    "Winning key: {} · role > exact > wildcard",
                    winner.as_deref().unwrap_or("none")
                )));
                lines.push(Line::from(format!(
                    "Effective backups: {}",
                    chain.iter().map(qualified).collect::<Vec<_>>().join(" → ")
                )));
                if chain.is_empty() {
                    lines.push(Line::from("No effective backup (empty/self-only chain, or exact-model privacy policy blocks substitution)."));
                }
            }
            Err(error) => lines.push(Line::from(format!("Invalid draft: {error:#}"))),
        }
        if self.reviewing {
            let mut review = vec![
                Line::styled(
                    "REVIEW — y accepts data/cost policy and saves; Esc edits",
                    ctx.theme.fg(Tok::Warning),
                ),
                Line::from(
                    "Conversation, source and tool results may be sent to a DIFFERENT PROVIDER.",
                ),
                Line::from(
                    "Pricing, retention and privacy terms may differ. No prices/health were checked.",
                ),
                Line::from(
                    "Configured endpoints/auth and exact-model privacy restrictions remain enforced.",
                ),
                Line::from(
                    "Trigger/return apply to all chains. Global saves keep Project overrides.",
                ),
                Line::default(),
            ];
            review.append(&mut lines);
            lines = review;
        }
        lines.extend([Line::default(), Line::from(self.notice.clone())]);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Backup models "),
                )
                .style(ctx.theme.fg(Tok::Text)),
            frame.area(),
        );
    }
}

fn qualified(selection: &ModelSelection) -> String {
    format!("{}/{}", selection.provider, selection.model)
}

#[cfg(test)]
mod tests;

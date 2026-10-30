//! Session-owned extension workspaces. Paint only consumes snapshots; callbacks run at the idle
//! boundary through the normal agent controller, retaining its tool and permission checks.
use super::Shell;
use crate::agent::Agent;
use crate::app::{App, Bridge, PAINT_EVERY};
use crate::runtime::extensions::{Location, OwnedView};
use crate::ui::{
    Rows,
    extension_views::{ViewEvent, ViewState},
    keys::Keymap,
    theme::Theme,
};
use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

#[derive(Default)]
pub(in crate::app) struct ExtensionViews {
    views: BTreeMap<String, ViewState>,
    active: Option<String>,
    hidden: BTreeSet<String>,
    focused: bool,
    pending: VecDeque<ViewEvent>,
    polled: BTreeMap<String, Instant>,
}
impl ExtensionViews {
    pub(in crate::app) fn sync(&mut self, snapshots: Vec<OwnedView>) -> bool {
        let mut changed = false;
        let mut seen = BTreeSet::new();
        for snapshot in snapshots.into_iter().take(64) {
            if snapshot.view.validate().is_err() {
                continue;
            }
            let key = format!("{}/{}", snapshot.plugin, snapshot.view.id);
            seen.insert(key.clone());
            if let Some(view) = self.views.get_mut(&key) {
                changed |= view.update(snapshot);
            } else {
                self.views.insert(key, ViewState::new(snapshot));
                changed = true;
            }
        }
        let before = self.views.len();
        self.views.retain(|key, _| seen.contains(key));
        self.hidden.retain(|key| seen.contains(key));
        changed |= self.views.len() != before;
        if self
            .active
            .as_ref()
            .is_none_or(|key| !self.views.contains_key(key) || self.hidden.contains(key))
        {
            let next = self
                .views
                .keys()
                .find(|key| !self.hidden.contains(*key))
                .cloned();
            changed |= next != self.active;
            self.active = next;
            self.focused = false;
        }
        changed
    }
    pub(in crate::app) fn rows(&mut self, width: usize, height: usize, theme: &Theme) -> Rows {
        let Some(view) = self.active.as_ref().and_then(|key| self.views.get_mut(key)) else {
            return Vec::new();
        };
        let cap = match view.snapshot().view.location {
            Location::Sidebar => height / 3,
            Location::Tab => height * 2 / 3,
        };
        view.rows(width, cap.min(24), theme, self.focused)
    }
    fn command(&mut self, args: &str) -> Result<String> {
        let mut words = args.split_whitespace();
        let Some(key) = words.next().filter(|s| *s != "list") else {
            return Ok(if self.views.is_empty() {
                "No extension views registered.".into()
            } else {
                self.views
                    .iter()
                    .map(|(key, view)| {
                        format!(
                            "{key} — {}{}",
                            view.snapshot().view.title,
                            if self.hidden.contains(key) {
                                " (hidden)"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        };
        if !self.views.contains_key(key) {
            bail!("unknown extension view {key}; use /extension-view list");
        }
        let action = words.next().unwrap_or("focus");
        if words.next().is_some() {
            bail!("usage: /extension-view [list | owner/id [show|hide|focus]]");
        }
        match action {
            "show" | "focus" => {
                self.hidden.remove(key);
                self.active = Some(key.into());
                self.focused = action == "focus";
            }
            "hide" => {
                self.hidden.insert(key.into());
                if self.active.as_deref() == Some(key) {
                    self.active = self
                        .views
                        .keys()
                        .find(|key| !self.hidden.contains(*key))
                        .cloned();
                    self.focused = false;
                }
            }
            _ => bail!("expected show, hide or focus"),
        }
        Ok(String::new())
    }
    pub(in crate::app) fn key(&mut self, key: KeyEvent, keys: &Keymap) -> bool {
        if !self.focused {
            return false;
        }
        let actions = keys.actions_for(&key);
        if key.code == KeyCode::Esc {
            self.focused = false;
            return true;
        }
        // Never shadow application actions (interrupt, exit, settings, etc.). Navigation and field
        // editing belong to this focused surface, including Enter rather than composer submission.
        if actions.iter().any(|a| a.id().starts_with("app.")) {
            return false;
        }
        if let Some(view) = self.active.as_ref().and_then(|id| self.views.get_mut(id))
            && let Some(event) = view.key(key, keys)
            && self.pending.len() < 64
        {
            self.pending.push_back(event);
        }
        true
    }
    fn next_event(&mut self) -> Option<ViewEvent> {
        while let Some(event) = self.pending.pop_front() {
            let name = event.params.get("name")?.as_str()?;
            let revision = event.params.get("revision")?.as_u64()?;
            let key = format!("{}/{name}", event.plugin);
            if self
                .views
                .get(&key)
                .is_some_and(|v| v.snapshot().revision == revision)
            {
                return Some(event);
            }
        }
        None
    }
}
impl Shell {
    pub(in crate::app) fn extension_view_command(&mut self, args: &str) -> Result<String> {
        self.extension_views.command(args)
    }
    /// Bounded event drain. No event is delivered after its snapshot was removed or superseded.
    pub(in crate::app) fn drain_extension_view_events(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
    ) -> bool {
        let Some(host) = agent.extension_host() else {
            return false;
        };
        let mut changed = self.extension_views.sync(host.views());
        for _ in 0..8 {
            let Some(event) = self.extension_views.next_event() else {
                break;
            };
            let result = {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                agent.run_extension_request(
                    &mut bridge,
                    &host,
                    &event.plugin,
                    "view.event",
                    event.params,
                )
            };
            if let Err(error) = result {
                self.note("Extension view", format!("{}: {error:#}", event.plugin));
            }
            changed = true;
            self.extension_views.sync(host.views());
        }
        // Only one worker is polled per pass; failing workers receive the same backoff.
        if let Some(plugin) = host.persistent_names().into_iter().find(|name| {
            self.extension_views
                .polled
                .get(name)
                .is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(500))
        }) {
            self.extension_views
                .polled
                .insert(plugin.clone(), Instant::now());
            let result = {
                let mut bridge = Bridge {
                    app,
                    shell: self,
                    interrupt: app.interrupt.clone(),
                    started: Instant::now(),
                    last_paint: Instant::now() - PAINT_EVERY,
                    failed: false,
                    pending: Vec::new(),
                    next_id: 0,
                };
                agent.run_extension_request(
                    &mut bridge,
                    &host,
                    &plugin,
                    "poll",
                    serde_json::json!({"workspace":app.layout.project_root}),
                )
            };
            if let Err(error) = result {
                self.note("Extension poll", format!("{plugin}: {error:#}"));
            }
            changed |= self.extension_views.sync(host.views());
        }
        changed
    }
}
#[cfg(test)]
mod tests;

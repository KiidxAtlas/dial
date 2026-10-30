//! Registered extension shortcuts use the existing chord parser and built-in conflict authority.
//! Public user overrides are separate from built-in actions, so unknown extension ids never alter
//! the built-in keymap. Plain typing and every built-in chord always win.
use super::Shell;
use crate::agent::{Agent, Ui};
use crate::app::{App, Bridge, PAINT_EVERY};
use crate::extensions::panel::{Element, Outcome, Panel, Value};
use crate::tools::integrations::plugin::Host;
use crate::ui::keys::{Chord, Keymap};
use anyhow::{Context, Result, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, Instant},
};

const FILE: &str = "extension-keybindings.toml";
#[derive(Default)]
pub(in crate::app) struct ExtensionKeys {
    bindings: Vec<(String, String, Chord)>,
    declarations: Vec<(String, String, Option<String>)>,
    warnings: Vec<String>,
    signature: String,
    checked: Option<Instant>,
}
fn overrides(path: &Path) -> Result<BTreeMap<String, String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text).context("invalid extension-keybindings.toml")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error.into()),
    }
}
fn layered_overrides(root: &Path, workspace: &Path) -> Result<BTreeMap<String, String>> {
    let mut values = overrides(&root.join(FILE))?;
    if root != workspace {
        values.extend(overrides(&workspace.join(FILE))?);
    }
    Ok(values)
}
fn admitted(chord: Chord, keys: &Keymap) -> Result<()> {
    ensure!(
        !matches!(chord.code, KeyCode::Char(_))
            || chord
                .mods
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        "shortcut would intercept typing"
    );
    let event = KeyEvent::new(chord.code, chord.mods);
    ensure!(
        keys.actions_for(&event).is_empty(),
        "shortcut conflicts with a built-in action"
    );
    Ok(())
}
impl ExtensionKeys {
    pub(in crate::app) fn refresh(
        &mut self,
        host: Option<&Host>,
        keys: &Keymap,
        root: &Path,
        workspace: &Path,
    ) -> bool {
        if self
            .checked
            .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
        {
            return false;
        }
        self.checked = Some(Instant::now());
        let mut declarations = Vec::new();
        if let Some(host) = host {
            for plugin in host.loaded_names().into_iter().take(64) {
                let Some(contributions) = host.contributions(&plugin) else {
                    continue;
                };
                for command in &contributions.commands {
                    let key = contributions
                        .keybindings
                        .iter()
                        .find(|b| b.command == command.name)
                        .map(|b| b.key.clone())
                        .or_else(|| command.keybinding.clone());
                    declarations.push((plugin.clone(), command.name.clone(), key));
                }
            }
        }
        let user = layered_overrides(root, workspace);
        let signature = format!("{declarations:?}{user:?}{keys:?}");
        if signature == self.signature {
            return false;
        }
        self.signature = signature;
        self.declarations = declarations;
        self.bindings.clear();
        self.warnings.clear();
        let user = match user {
            Ok(user) => user,
            Err(error) => {
                self.warnings.push(format!("{error:#}"));
                BTreeMap::new()
            }
        };
        for (plugin, command, default) in &self.declarations {
            let id = format!("{plugin}/{command}");
            let Some(value) = user.get(&id).or(default.as_ref()).filter(|s| !s.is_empty()) else {
                continue;
            };
            let result = Chord::parse(value)
                .map_err(anyhow::Error::msg)
                .and_then(|chord| {
                    admitted(chord, keys)?;
                    ensure!(
                        !self.bindings.iter().any(|(_, _, taken)| *taken == chord),
                        "shortcut conflicts with another extension"
                    );
                    Ok(chord)
                });
            match result {
                Ok(chord) => self.bindings.push((plugin.clone(), command.clone(), chord)),
                Err(error) => self.warnings.push(format!("{id}: {error:#}")),
            }
        }
        for id in user.keys() {
            if !self
                .declarations
                .iter()
                .any(|(p, c, _)| format!("{p}/{c}") == *id)
            {
                self.warnings
                    .push(format!("{id}: extension command is not registered"));
            }
        }
        true
    }
    pub(in crate::app) fn key(&self, key: KeyEvent, keys: &Keymap) -> Option<String> {
        if key.kind != KeyEventKind::Press || !keys.actions_for(&key).is_empty() {
            return None;
        }
        self.bindings
            .iter()
            .find(|(_, _, chord)| chord.matches(&key))
            .map(|(_, command, _)| format!("/{command}"))
    }
}
impl Shell {
    pub(in crate::app) fn extension_keybindings_command(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        args: &str,
    ) -> Result<String> {
        let host = agent.extension_host();
        self.extension_keys.checked = None;
        self.extension_keys.refresh(
            host.as_deref(),
            &self.keys,
            &app.layout.user_dir,
            &app.layout.project_dir,
        );
        let (scope, args) = args.trim().split_once(' ').unwrap_or((args.trim(), ""));
        let (workspace, action) = match scope {
            "workspace" => (true, args.trim()),
            "global" => (false, args.trim()),
            _ => (false, scope),
        };
        if action == "list" {
            let mut lines: Vec<_> = self
                .extension_keys
                .bindings
                .iter()
                .map(|(p, c, key)| format!("{p}/{c}: {}", key.display()))
                .collect();
            lines.extend(self.extension_keys.warnings.clone());
            return Ok(lines.join("\n"));
        }
        ensure!(
            action.is_empty(),
            "usage: /extension-keybindings [global|workspace] [list]"
        );
        let names: Vec<_> = self
            .extension_keys
            .declarations
            .iter()
            .map(|(p, c, _)| format!("{p}/{c}"))
            .collect();
        ensure!(!names.is_empty(), "no extension commands registered");
        ensure!(
            names.len() <= 48,
            "too many extension commands for one shortcut form; edit {} directly",
            app.layout.user_dir.join(FILE).display()
        );
        let panel = Panel { title: "Extension keybindings".into(), generation: 1, elements: vec![
            Element::Text("Built-in shortcuts always win. Use Ctrl/Alt chords; an empty shortcut disables the binding. Current bindings and conflicts: /extension-keybindings list".into()),
            Element::Select { id: "command".into(), label: "Command".into(), options: names.clone(), selected: 0 },
            Element::Input { id: "key".into(), label: "Shortcut (e.g. Ctrl+Alt+B)".into(), value: String::new() },
            Element::Button { id: "save".into(), label: "Save override".into(), primary: true },
        ] };
        let outcome = {
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
            bridge.run_panel(&panel, &app.interrupt)
        };
        match outcome {
            Outcome::Cancelled => Ok(String::new()),
            Outcome::Unsupported(reason) => anyhow::bail!("shortcut form unavailable: {reason}"),
            Outcome::Submitted { values, .. } => {
                let Some(Value::Choice(index)) = values.get("command") else {
                    anyhow::bail!("missing command");
                };
                let Some(Value::Text(key)) = values.get("key") else {
                    anyhow::bail!("missing shortcut");
                };
                let name = names.get(*index).context("invalid command")?;
                if !key.trim().is_empty() {
                    let chord = Chord::parse(key).map_err(anyhow::Error::msg)?;
                    admitted(chord, &self.keys)?;
                    ensure!(
                        !self
                            .extension_keys
                            .bindings
                            .iter()
                            .any(|(p, c, taken)| format!("{p}/{c}") != *name && *taken == chord),
                        "shortcut conflicts with another extension"
                    );
                }
                let path = if workspace {
                    &app.layout.project_dir
                } else {
                    &app.layout.user_dir
                }
                .join(FILE);
                let mut user = overrides(&path)?;
                user.insert(name.clone(), key.trim().into());
                crate::platform::atomic::write_private(
                    &path,
                    toml::to_string_pretty(&user)?.as_bytes(),
                )?;
                self.extension_keys.checked = None;
                self.extension_keys.refresh(
                    host.as_deref(),
                    &self.keys,
                    &app.layout.user_dir,
                    &app.layout.project_dir,
                );
                Ok(format!("Saved shortcut for {name}."))
            }
        }
    }
}
#[cfg(test)]
mod tests;

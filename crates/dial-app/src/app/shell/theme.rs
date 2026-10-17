//! The `/theme` command.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::drive::Step;
use crate::app::shell::Shell;
use crate::config::settings_store::{Change, Scope};
use anyhow::Result;

impl Shell {
    pub(super) fn theme_command(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        args: &str,
    ) -> Result<Step> {
        if args.is_empty() {
            let active = self.themes.current().name().to_owned();
            // Grouped one line per kind (wrapped by the transcript) instead of one name per row.
            let (mut dark, mut light, mut custom) = (Vec::new(), Vec::new(), Vec::new());
            for entry in self.themes.names() {
                let name = if entry.name == active {
                    format!("● {}", entry.name)
                } else {
                    entry.name.clone()
                };
                if let Some(error) = entry.error {
                    custom.push(format!("{name} (cannot load: {error})"));
                } else if !entry.builtin {
                    custom.push(name);
                } else if crate::ui::theme::Theme::builtin(&entry.name)
                    .is_some_and(|theme| !theme.is_dark())
                {
                    light.push(name);
                } else {
                    dark.push(name);
                }
            }
            let mut lines = vec![format!("Current: {active}")];
            for (kind, names) in [("Dark", dark), ("Light", light), ("Custom", custom)] {
                if !names.is_empty() {
                    lines.push(format!("{kind}: {}", names.join(" · ")));
                }
            }
            lines.push(String::new());
            lines.push(
                "Switch with /theme <name> (saved globally), or preview live in /settings.".into(),
            );
            lines.push(format!(
                "Custom themes are JSON files in {}; they reload live.",
                self.themes.dir().display()
            ));
            self.note("Themes", lines.join("\n"));
            return Ok(Step::Done);
        }
        if self.themes.names().iter().all(|entry| entry.name != args) && args != "auto" {
            self.note(
                "Theme",
                format!("unknown theme {args:?}; /theme lists them"),
            );
            return Ok(Step::Done);
        }
        let changes = [Change::Set {
            path: "ui.theme".into(),
            value: args.to_owned().into(),
        }];
        match self.persist(app, agent, &changes, Scope::Global) {
            Ok(step) => {
                self.note("Theme", format!("switched to {args}"));
                Ok(step)
            }
            Err(error) => {
                self.note("Theme", format!("not changed: {error:#}"));
                Ok(Step::Done)
            }
        }
    }
}

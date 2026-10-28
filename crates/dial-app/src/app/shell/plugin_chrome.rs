//! Plugin header, status, footer and title slots, plus composer autocomplete from plugins. Slots are
//! read once per idle pass. Autocomplete runs on a worker thread, bounded by the plugin timeout, and
//! its answer is merged on a later pass, so typing never waits on a plugin.

use crate::agent::Agent;
use crate::app::shell::Shell;
use crate::tools::integrations::plugin::protocol::{Slot, Suggestion, UiSlot};
use crate::ui::composer::{CommandEntry, CommandKind};
use crate::ui::plugin_slots::{Placement, PluginSlots};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

/// One autocomplete answer: the query it was asked for, and the entries it produced.
type Answer = (String, Vec<CommandEntry>);

/// Background state for plugin autocomplete: the query last sent, and the channel answers return on.
pub(in crate::app) struct PluginChrome {
    sent: Option<String>,
    tx: mpsc::Sender<Answer>,
    rx: mpsc::Receiver<Answer>,
}

impl Default for PluginChrome {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { sent: None, tx, rx }
    }
}

impl Shell {
    /// Reads plugin chrome and lands autocomplete answers. Returns `true` when the composer or the
    /// title changed, so the caller repaints. Never waits on a plugin.
    pub(in crate::app) fn refresh_plugin_chrome(&mut self, agent: &Agent<'_>) -> bool {
        let host = agent.extension_host();
        let slots = host
            .as_ref()
            .map(|host| slots_from(&host.ui_slots()))
            .unwrap_or_default();
        let mut changed = self.composer.set_plugin_slots(slots);

        let query = self.composer.plugin_query().map(str::to_owned);
        if query != self.plugin_chrome.sent {
            self.plugin_chrome.sent = query.clone();
            if let (Some(query), Some(host)) = (query, host) {
                let tx = self.plugin_chrome.tx.clone();
                spawn_lookup(tx, query, move |query| {
                    host.autocomplete(query, &AtomicBool::new(false))
                });
            }
        }

        while let Ok((query, entries)) = self.plugin_chrome.rx.try_recv() {
            changed |= self.composer.set_plugin_suggestions(&query, entries);
        }
        changed
    }
}

/// Runs `lookup` on a detached worker and sends its answer on `tx`. The caller returns at once; a
/// slow or hung plugin only delays its own answer, which the composer drops if the query moved on.
fn spawn_lookup<F>(tx: mpsc::Sender<Answer>, query: String, lookup: F)
where
    F: FnOnce(&str) -> Vec<Suggestion> + Send + 'static,
{
    std::thread::spawn(move || {
        let entries = suggestion_entries(lookup(&query));
        // The receiver lives as long as the shell; a closed channel means the session ended.
        let _ = tx.send((query, entries));
    });
}

/// Maps plugin slot contributions onto the composer's placements. Editor-mode and other slots are
/// not chrome and are skipped.
fn slots_from(slots: &[(String, UiSlot)]) -> PluginSlots {
    PluginSlots::collect(slots.iter().map(|(_, slot)| {
        let placement = match slot.slot {
            Slot::Header => Placement::Header,
            Slot::Footer => Placement::Footer,
            Slot::Status => Placement::Status,
            Slot::Title => Placement::Title,
        };
        (placement, slot.lines.as_slice())
    }))
}

/// Plugin suggestions as command-menu entries. A leading slash is dropped, since the menu adds it.
/// Values containing whitespace are kept; the composer single-quotes them when one is inserted.
pub(in crate::app) fn suggestion_entries(items: Vec<Suggestion>) -> Vec<CommandEntry> {
    items
        .into_iter()
        .map(|item| {
            let label = item.label.trim();
            let description = item.description.trim();
            let description = match (label.is_empty(), description.is_empty()) {
                (true, _) => description.to_owned(),
                (false, true) => label.to_owned(),
                (false, false) => format!("{label} · {description}"),
            };
            CommandEntry {
                name: item.value.trim_start_matches('/').to_owned(),
                description,
                kind: CommandKind::Extension,
                args_hint: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;

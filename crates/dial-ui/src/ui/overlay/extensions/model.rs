//! Pure state for the `/extensions` browser: tabs, rows, search, cursor and the confirm step.
//!
//! Nothing here touches the disk, the network or config. The controller in `dial-app/src/app/commands/` builds the
//! rows and turns confirmed [`Intent`]s into work.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tab {
    Mcp,
    Skills,
}

/// Number of tabs; sizes the per-tab arrays in [`Browser`].
const TABS: usize = 2;
const _: () = assert!(Tab::ALL.len() == TABS);

impl Tab {
    pub const ALL: [Tab; TABS] = [Tab::Mcp, Tab::Skills];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Mcp => "MCP servers",
            Tab::Skills => "Skills",
        }
    }

    fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Where a row came from. Decides which actions the details pane offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Already configured or installed in Dial.
    Installed,
    /// Built-in curated catalog entry, not installed yet.
    Curated,
    /// Found through the Smithery registry or a marketplace index.
    Remote,
    /// Configured for another coding agent (Claude, Codex, Cursor…); importing copies it.
    Foreign,
}

/// Health of an installed item, or availability of a catalog item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Connected,
    NeedsSignIn,
    Failing,
    Disabled,
    Enabled,
    Available,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Connected => "connected",
            State::NeedsSignIn => "needs sign-in",
            State::Failing => "failing",
            State::Disabled => "disabled",
            State::Enabled => "enabled",
            State::Available => "available",
        }
    }
}

/// One line in a tab. Built by the controller; the UI only displays it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Stable key: server name or skill source.
    pub id: String,
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub origin: Origin,
    pub state: State,
    /// Detail lines shown in the details pane: what it does, tools, command, install location.
    pub details: Vec<String>,
}

impl Row {
    fn matches(&self, needle: &str) -> bool {
        let hit = |s: &str| s.to_lowercase().contains(needle);
        hit(&self.id) || hit(&self.title) || hit(&self.summary) || self.tags.iter().any(|t| hit(t))
    }
}

/// A user-confirmed action. Serialized into `extensions:<json>` so the controller can run it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Intent {
    /// Install a curated MCP server into Dial's user config and health-check it.
    InstallCuratedMcp {
        id: String,
    },
    /// Install a server from the official registry or the curated list. `values` fill its
    /// required environment variables; the controller asks for them when they are missing.
    InstallMcp {
        id: String,
        #[serde(default)]
        values: BTreeMap<String, String>,
    },
    ToggleMcp {
        id: String,
        enable: bool,
    },
    RemoveMcp {
        id: String,
    },
    TestMcp {
        id: String,
    },
    /// Remove an installed skill package (both its skills and its lock entry).
    RemoveSkill {
        id: String,
    },
    /// Copy a server that another coding agent has configured into Dial's config.
    ImportMcp {
        id: String,
    },
    /// Install a Smithery registry server. Asks for the API key first when none is stored.
    SmitheryInstall {
        name: String,
    },
    /// Check and save a Smithery API key, then ask for the server's settings.
    SmitheryKey {
        name: String,
        key: String,
    },
    /// Install a Smithery server with the settings the user entered.
    SmitheryDeploy {
        name: String,
        values: BTreeMap<String, String>,
    },
    /// Read a catalog page; no package execution or installation.
    CatalogBrowse {
        tab: Tab,
        query: String,
        page: usize,
    },
    /// Install a skill package from a skill listing; `source` is `repo[@ref][#path]`.
    InstallSkill {
        source: String,
    },
}

impl Intent {
    /// One-line summary shown on the confirm step.
    pub fn describe(&self) -> String {
        match self {
            Intent::InstallCuratedMcp { id } => format!("Install MCP server `{id}`"),
            Intent::InstallMcp { id, .. } => format!("Install MCP server `{id}`"),
            Intent::ImportMcp { id } => format!("Import MCP server `{id}` into Dial's config"),
            Intent::ToggleMcp { id, enable: true } => format!("Enable MCP server `{id}`"),
            Intent::ToggleMcp { id, enable: false } => format!("Disable MCP server `{id}`"),
            Intent::RemoveMcp { id } => format!("Remove MCP server `{id}`"),
            Intent::TestMcp { id } => format!("Test MCP server `{id}`"),
            Intent::RemoveSkill { id } => format!("Remove skill package `{id}`"),
            Intent::SmitheryInstall { name } => format!("Install Smithery server `{name}`"),
            Intent::SmitheryKey { .. } => "Check and save the Smithery API key".to_owned(),
            Intent::SmitheryDeploy { name, .. } => {
                format!("Install Smithery server `{name}` with these settings")
            }
            Intent::CatalogBrowse { tab, query, page } => {
                format!("Browse {}: `{query}` (page {page})", tab.label())
            }
            Intent::InstallSkill { source } => format!("Install skill package `{source}`"),
        }
    }

    /// Whether the action changes files, runs a downloaded command, or removes or disables something.
    /// Those wait for an explicit Enter. Enabling and testing run at once.
    pub fn needs_confirm(&self) -> bool {
        matches!(
            self,
            Intent::InstallCuratedMcp { .. }
                | Intent::ImportMcp { .. }
                | Intent::InstallMcp { .. }
                | Intent::RemoveMcp { .. }
                | Intent::RemoveSkill { .. }
                | Intent::ToggleMcp { enable: false, .. }
                | Intent::SmitheryInstall { .. }
                | Intent::SmitheryDeploy { .. }
                | Intent::InstallSkill { .. }
        )
    }
}

/// Progress written by the install worker and read by the overlay on each tick.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobProgress {
    /// Label of the running job, or `None` when idle.
    pub label: Option<String>,
    pub lines: Vec<String>,
    /// Final outcome: whether it succeeded and what to tell the user.
    pub result: Option<(bool, String)>,
    /// A form for the user to fill in. The overlay opens it on its next tick.
    pub form: Option<Form>,
    pub catalog: Option<CatalogUpdate>,
}

/// A small form shown in place of the list: the Smithery key, a server's settings, or a registry search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub kind: FormKind,
    pub title: String,
    /// One line under the title.
    pub note: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    /// Why the last submit was refused, shown above the fields.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormKind {
    /// Smithery API key. `name` is the server to install once the key is accepted.
    Key { name: String },
    /// Settings for the Smithery server `name`.
    Config { name: String },
    /// Values for the curated or official-registry server `id`.
    McpInputs { id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub key: String,
    pub label: String,
    /// Short description of the setting, shown beside it.
    pub hint: String,
    pub required: bool,
    /// Shown as dots, never in clear text.
    pub secret: bool,
    pub value: String,
}

impl Form {
    pub fn key(name: &str, error: Option<String>) -> Self {
        Self {
            kind: FormKind::Key {
                name: name.to_owned(),
            },
            title: "Smithery API key".to_owned(),
            note: "Installing from Smithery needs an API key. It is checked, then saved for next time."
                .to_owned(),
            fields: vec![Field {
                key: "api_key".to_owned(),
                label: "API key".to_owned(),
                hint: String::new(),
                required: true,
                secret: true,
                value: String::new(),
            }],
            focus: 0,
            error,
        }
    }

    pub fn config(
        name: &str,
        title: &str,
        note: &str,
        fields: Vec<Field>,
        error: Option<String>,
    ) -> Self {
        Self {
            kind: FormKind::Config {
                name: name.to_owned(),
            },
            title: title.to_owned(),
            note: note.to_owned(),
            fields,
            focus: 0,
            error,
        }
    }

    /// A form for the values an MCP server needs (environment variables), keyed by listing id.
    pub fn mcp_inputs(
        id: &str,
        title: &str,
        note: &str,
        fields: Vec<Field>,
        error: Option<String>,
    ) -> Self {
        Self {
            kind: FormKind::McpInputs { id: id.to_owned() },
            title: title.to_owned(),
            note: note.to_owned(),
            fields,
            focus: 0,
            error,
        }
    }

    /// Label of the first required field that is still empty.
    pub fn missing_required(&self) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.required && f.value.trim().is_empty())
            .map(|f| f.label.as_str())
    }

    /// Trimmed, non-empty values keyed by field key.
    pub fn values(&self) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .filter(|f| !f.value.trim().is_empty())
            .map(|f| (f.key.clone(), f.value.trim().to_owned()))
            .collect()
    }

    pub fn focus_next(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + 1) % self.fields.len();
        }
    }

    pub fn focus_prev(&mut self) {
        if !self.fields.is_empty() {
            self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
        }
    }

    /// Types one character into the focused field.
    pub fn push(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.push(c);
            self.error = None;
        }
    }

    /// Deletes the last character of the focused field.
    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.focus) {
            field.value.pop();
            self.error = None;
        }
    }
}

/// Shared between the controller (writer) and the overlay (reader).
pub type SharedProgress = std::sync::Arc<std::sync::Mutex<JobProgress>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CatalogState {
    pub query: String,
    pub page: usize,
    pub more: bool,
    pub loading: bool,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogUpdate {
    pub tab: Tab,
    pub query: String,
    pub page: usize,
    pub rows: Vec<Row>,
    pub more: bool,
    pub error: Option<String>,
    pub message: String,
}

pub type SharedCatalog = std::sync::Arc<std::sync::Mutex<Option<CatalogUpdate>>>;

/// Progress of the one running install, shown in the overlay while it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    Idle,
    Running {
        label: String,
        lines: Vec<String>,
    },
    Done {
        label: String,
        ok: bool,
        message: String,
    },
}

#[derive(Debug)]
pub struct Browser {
    tab: Tab,
    query: String,
    searching: bool,
    rows: [Vec<Row>; TABS],
    cursor: [usize; TABS],
    catalog: [CatalogState; TABS],
    /// Intent waiting for the user's yes/no.
    pub confirm: Option<Intent>,
    pub job: Job,
    pub message: String,
    pub details_open: bool,
    /// Form shown in place of the list while the user fills something in.
    pub form: Option<Form>,
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            tab: Tab::Mcp,
            query: String::new(),
            searching: false,
            rows: std::array::from_fn(|_| Vec::new()),
            cursor: [0; TABS],
            catalog: std::array::from_fn(|_| CatalogState::default()),
            confirm: None,
            job: Job::Idle,
            message: String::new(),
            details_open: false,
            form: None,
        }
    }
}

impl Browser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tab(&self) -> Tab {
        self.tab
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn searching(&self) -> bool {
        self.searching
    }

    /// Replace one tab's rows, keeping the cursor on the same id when it is still present.
    pub fn set_rows(&mut self, tab: Tab, rows: Vec<Row>) {
        let i = tab.index();
        let keep = self.selected_in(tab).map(|r| r.id.clone());
        self.rows[i] = rows;
        let visible = self.visible(tab);
        self.cursor[i] = keep
            .and_then(|id| visible.iter().position(|r| r.id == id))
            .unwrap_or(0);
    }

    pub fn rows(&self, tab: Tab) -> &[Row] {
        &self.rows[tab.index()]
    }

    pub fn catalog(&self, tab: Tab) -> &CatalogState {
        &self.catalog[tab.index()]
    }

    pub fn begin_catalog(&mut self, tab: Tab, query: String, page: usize) -> Intent {
        let state = &mut self.catalog[tab.index()];
        state.loading = true;
        state.message = format!("Loading {} catalog…", tab.label());
        Intent::CatalogBrowse { tab, query, page }
    }

    pub fn apply_catalog(&mut self, update: CatalogUpdate) {
        let tab = update.tab;
        let state = &mut self.catalog[tab.index()];
        state.loading = false;
        if let Some(error) = update.error {
            // Stop paging so scrolling does not retry a failing page on every key; `r` restarts.
            state.more = false;
            state.message =
                format!("Catalog unavailable: {error}. r retries; cached rows retained.");
            return;
        }
        state.query = update.query;
        state.page = update.page;
        state.more = update.more;
        state.message = update.message;
        let mut rows: Vec<Row> = self
            .rows(tab)
            .iter()
            .filter(|row| update.page > 1 || row.origin != Origin::Remote)
            .cloned()
            .collect();
        for row in update.rows {
            if !rows.iter().any(|existing| existing.id == row.id) {
                rows.push(row);
            }
        }
        self.set_rows(tab, rows);
    }

    /// Locally filter cached rows, but preserve MCP results returned for this exact
    /// search: registries may match metadata not included in the displayed row.
    pub fn visible(&self, tab: Tab) -> Vec<&Row> {
        let needle = self.query.trim().to_lowercase();
        let remote_search =
            tab == Tab::Mcp && self.catalog(tab).query.trim().to_lowercase() == needle;
        self.rows[tab.index()]
            .iter()
            .filter(|row| {
                needle.is_empty()
                    || row.matches(&needle)
                    || (remote_search && row.origin == Origin::Remote)
            })
            .collect()
    }

    /// Whether the MCP tab's query has not been sent to the catalogs yet. Typing only filters
    /// the rows already loaded (the first browse page), so a server missing from them may still
    /// be listed; the catalogs are searched when the query is confirmed.
    pub fn catalog_search_pending(&self) -> bool {
        let needle = self.query.trim().to_lowercase();
        let state = self.catalog(Tab::Mcp);
        self.tab == Tab::Mcp
            && !needle.is_empty()
            && !state.loading
            && state.query.trim().to_lowercase() != needle
    }

    /// Match counts for every tab while a query is active, e.g. "MCP servers 1 · Skills 0".
    /// Empty when no query is set.
    pub fn match_summary(&self) -> String {
        if self.query.trim().is_empty() {
            return String::new();
        }
        Tab::ALL
            .iter()
            .map(|tab| format!("{} {}", tab.label(), self.visible(*tab).len()))
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn cursor(&self) -> usize {
        self.cursor[self.tab.index()]
    }

    pub fn selected(&self) -> Option<&Row> {
        self.selected_in(self.tab)
    }

    fn selected_in(&self, tab: Tab) -> Option<&Row> {
        self.visible(tab).get(self.cursor[tab.index()]).copied()
    }

    pub fn next_tab(&mut self) {
        self.switch((self.tab.index() + 1) % Tab::ALL.len());
    }

    pub fn prev_tab(&mut self) {
        self.switch((self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len());
    }

    fn switch(&mut self, index: usize) {
        self.tab = Tab::ALL[index];
        self.details_open = false;
        self.confirm = None;
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = self.visible(self.tab).len();
        if len == 0 {
            self.cursor[self.tab.index()] = 0;
            return;
        }
        let at = self.cursor[self.tab.index()] as isize + delta;
        self.cursor[self.tab.index()] = at.clamp(0, len as isize - 1) as usize;
    }

    /// Open search. Typing filters every tab live.
    pub fn start_search(&mut self) {
        self.searching = true;
        self.details_open = false;
    }

    /// Leave search, keeping the filter applied.
    pub fn end_search(&mut self) {
        self.searching = false;
    }

    /// Clear the filter and leave search.
    pub fn clear_search(&mut self) {
        self.query.clear();
        self.searching = false;
        self.clamp_cursors();
    }

    pub fn push_query(&mut self, c: char) {
        self.query.push(c);
        self.clamp_cursors();
    }

    pub fn pop_query(&mut self) {
        self.query.pop();
        self.clamp_cursors();
    }

    fn clamp_cursors(&mut self) {
        for tab in Tab::ALL {
            let len = self.visible(tab).len();
            let i = tab.index();
            self.cursor[i] = self.cursor[i].min(len.saturating_sub(1));
        }
    }

    pub fn toggle_details(&mut self) {
        self.details_open = !self.details_open;
    }

    /// Ask for confirmation. Actions that need none are returned at once.
    pub fn request(&mut self, intent: Intent) -> Option<Intent> {
        if intent.needs_confirm() {
            self.confirm = Some(intent);
            None
        } else {
            Some(intent)
        }
    }

    /// Yes on the confirm step: take the intent to run.
    pub fn accept(&mut self) -> Option<Intent> {
        self.confirm.take()
    }

    pub fn decline(&mut self) {
        self.confirm = None;
    }

    #[cfg(test)]
    pub fn job_running(&self) -> bool {
        matches!(self.job, Job::Running { .. })
    }

    pub fn start_job(&mut self, label: impl Into<String>) {
        self.job = Job::Running {
            label: label.into(),
            lines: Vec::new(),
        };
    }

    #[cfg(test)]
    pub fn push_job_line(&mut self, line: impl Into<String>) {
        if let Job::Running { lines, .. } = &mut self.job {
            lines.push(line.into());
        }
    }

    pub fn finish_job(&mut self, ok: bool, message: impl Into<String>) {
        let label = match &self.job {
            Job::Running { label, .. } => label.clone(),
            _ => String::new(),
        };
        self.job = Job::Done {
            label,
            ok,
            message: message.into(),
        };
    }
}

#[cfg(test)]
mod tests;

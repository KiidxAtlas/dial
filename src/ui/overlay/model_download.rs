//! The Get models group of `/models`: the downloaded MLX and GGUF models, Hugging Face search for more,
//! a GGUF quantization picker, the curated Ollama embedding models, and cancellable background downloads.
//!
//! Nothing here fetches weights on its own. Searching, listing and reading Ollama's model list run on
//! worker threads. Weights are fetched only after Enter on a result (or on a GGUF quantization the user
//! picked), through `llm::download::ensure_local_model_with`. Esc cancels a running download, and
//! dropping the component (closing `/models`) cancels it too. Enter on an installed model and on an
//! embedding model comes back as a [`Step`] for the models panel to turn into an event: the same
//! assignment `/models` makes, and the same setup `/setup embeddings:<model> yes` runs.
use super::settings::{filled, fit, hint, pair_label, selected_fill, typed_char};
use crate::cli::local_models::{self, Dashboard, ModelRow, Settings};
use crate::llm::catalog::ModelEntry;
use crate::llm::download::{self, Choice, Family, Hit, Quant};
use crate::llm::local_fit::Fit;
use crate::llm::model_config::ModelSelection;
use crate::setup::embeddings;
use crate::storage::paths::Layout;
use crate::ui::{
    keys::{Action, Keymap},
    text,
    theme::{Theme, Tok},
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::time::Instant;

/// What a key did, for the models panel to act on.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Not a key this group uses; the panel handles it.
    Ignored,
    Handled,
    /// Make the installed model at this path the main model.
    UseModel(String),
    /// Set up this curated embedding model, as `/setup embeddings:<model> yes` does.
    SetupEmbedding(String),
    /// Assign this discovered, served embedding model to the embedding role.
    UseEmbedding(ModelSelection),
    /// Leave the group: the panel focuses its groups column (or closes when that column is hidden).
    Back,
}

/// What the panel reads and downloads with, taken from the same layout and config as `/models`.
struct Source {
    layout: Layout,
    settings: Settings,
    python: PathBuf,
}

/// The shelves of the group: one Hugging Face weight format, or the curated embedding models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shelf {
    Hub(Family),
    Embedding,
}

const SHELVES: [Shelf; 3] = [
    Shelf::Hub(Family::Mlx),
    Shelf::Hub(Family::Gguf),
    Shelf::Embedding,
];

impl Shelf {
    fn label(self) -> &'static str {
        match self {
            Shelf::Hub(family) => family.label(),
            Shelf::Embedding => "Embedding",
        }
    }

    fn next(self) -> Self {
        match self {
            Shelf::Hub(Family::Mlx) => Shelf::Hub(Family::Gguf),
            Shelf::Hub(Family::Gguf) => Shelf::Embedding,
            Shelf::Embedding => Shelf::Hub(Family::Mlx),
        }
    }

    fn prev(self) -> Self {
        match self {
            Shelf::Hub(Family::Mlx) => Shelf::Embedding,
            Shelf::Hub(Family::Gguf) => Shelf::Hub(Family::Mlx),
            Shelf::Embedding => Shelf::Hub(Family::Gguf),
        }
    }

    fn family(self) -> Option<Family> {
        match self {
            Shelf::Hub(family) => Some(family),
            Shelf::Embedding => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Query,
    Results,
}

/// A row the cursor can rest on, in display order.
#[derive(Clone, Copy)]
enum Pick {
    /// Index into the dashboard's installed models.
    Installed(usize),
    /// Index into the current Hugging Face hits.
    Hit(usize),
    Embedding(&'static embeddings::Model),
    /// Index into the discovered embedding models.
    Discovered(usize),
}

struct Picker {
    repo: String,
    quants: Vec<Quant>,
    cursor: usize,
}

/// Background work the group is waiting on. At most one runs at a time.
enum Task {
    /// Listing a repository's files: sizes the download and, for GGUF, offers the quantizations.
    Inspect {
        repo: String,
        family: Family,
        rx: Receiver<Result<Vec<(String, u64)>, String>>,
    },
    Download(Download),
}

struct Download {
    repo: String,
    family: Family,
    /// Bytes the download is expected to fetch; zero when the listing did not report sizes.
    expected: u64,
    /// Bytes already in the Hugging Face cache for the repository when the download started.
    baseline: u64,
    started: Instant,
    cancel: Arc<AtomicBool>,
    lines: Receiver<String>,
    last: String,
    done: Receiver<Result<String, String>>,
}

/// State of the Get models group. Owned by `ModelOverlay`, so a download keeps running while the user
/// browses other groups; closing the panel drops it and cancels a running download.
pub struct ModelDownloads {
    source: Option<Source>,
    dashboard: Dashboard,
    shelf: Shelf,
    focus: Focus,
    query: String,
    /// Index into [`ModelDownloads::picks`].
    cursor: usize,
    hits: Vec<Hit>,
    /// True while `hits` is the curated list (shown for an empty query), not a search result.
    curated: bool,
    /// Status of the last search, e.g. "12 results" or the error.
    status: String,
    search: Option<Receiver<Result<Vec<Hit>, String>>>,
    browse_attempted: bool,
    metadata: Option<(String, Receiver<Result<Hit, String>>)>,
    metadata_attempted: BTreeSet<String>,
    /// Quantizations of a GGUF repository waiting for the user's choice.
    picker: Option<Picker>,
    task: Option<Task>,
    /// One-line result of the last action, shown in place of the status when there is no download.
    message: String,
    /// Ollama's pulled models, read on a worker thread when the Embedding shelf is shown. `None` while
    /// unknown or when Ollama is not reachable.
    pulled: Option<Vec<String>>,
    pulled_rx: Option<Receiver<Result<Vec<String>, String>>>,
    /// Embedding models discovery found on any provider, listed before the curated catalog.
    discovered: Vec<ModelEntry>,
    /// Set after a download finishes; the panel takes it and asks the controller to rescan.
    rescan: bool,
}

impl ModelDownloads {
    /// A group with no installed models read and no downloads: downloads report unavailable.
    pub fn idle() -> Self {
        Self::with(
            Dashboard {
                generated_ms: 0,
                memory_bytes: None,
                memory_source: "unknown",
                stats: "missing",
                models: Vec::new(),
            },
            None,
        )
    }

    /// Reads the installed models now. Downloads and the refresh after one use `python` and `layout`.
    pub fn open(layout: &Layout, settings: Settings, python: PathBuf) -> Self {
        let dashboard = local_models::collect(layout, &settings, crate::stats::now_ms());
        let source = Source {
            layout: layout.clone(),
            settings,
            python,
        };
        Self::with(dashboard, Some(source))
    }

    /// A fixed snapshot with no downloads, on the MLX shelf.
    #[cfg(test)]
    pub fn fixed(dashboard: Dashboard) -> Self {
        let mut group = Self::with(dashboard, None);
        group.shelf = Shelf::Hub(Family::Mlx);
        group.show_curated();
        group
    }

    fn with(dashboard: Dashboard, source: Option<Source>) -> Self {
        let family = download::default_family();
        Self {
            source,
            dashboard,
            shelf: Shelf::Hub(family),
            focus: Focus::Results,
            query: String::new(),
            cursor: 0,
            hits: download::recommended(family),
            curated: true,
            status: String::new(),
            search: None,
            browse_attempted: false,
            metadata: None,
            metadata_attempted: BTreeSet::new(),
            picker: None,
            task: None,
            message: String::new(),
            pulled: None,
            pulled_rx: None,
            discovered: Vec::new(),
            rescan: false,
        }
    }

    /// Shows the embedding shelf without reading Ollama's model list (tests only).
    #[cfg(test)]
    pub fn show_embedding_for_test(&mut self) {
        self.shelf = Shelf::Embedding;
    }

    /// Whether a download is running (tests only).
    #[cfg(test)]
    pub fn is_running(&self) -> bool {
        self.is_downloading()
    }

    /// Installed MLX and GGUF models, for the group's count.
    pub fn installed_count(&self) -> usize {
        self.dashboard.models.len()
    }

    /// Whether the GGUF quantization picker is open; it keeps its own keys.
    pub fn has_picker(&self) -> bool {
        self.picker.is_some()
    }

    /// The embedding models discovery found (Ollama, LM Studio, other servers and local checkpoints).
    /// The Embedding shelf lists them before the curated catalog; Enter assigns a served one.
    pub fn set_embedding_models(&mut self, models: &[ModelEntry]) {
        self.discovered = models
            .iter()
            .filter(|entry| entry.is_embedding())
            .cloned()
            .collect();
    }

    /// Whether a download finished since the last call, so the controller should rescan.
    pub fn take_rescan(&mut self) -> bool {
        std::mem::take(&mut self.rescan)
    }

    /// Re-reads the installed models (and Ollama's list on the Embedding shelf). Explicit refresh only.
    pub fn reload(&mut self) {
        self.browse_attempted = false;
        self.metadata_attempted.clear();
        self.metadata = None;
        if let Some(source) = &self.source {
            self.dashboard =
                local_models::collect(&source.layout, &source.settings, crate::stats::now_ms());
        }
        if self.shelf == Shelf::Embedding {
            self.refresh_pulled();
        }
    }

    /// Load catalog metadata only while this pane is visible. Never fetches model weights.
    pub fn load_visible(&mut self) {
        if self.source.is_none() || self.picker.is_some() || self.task.is_some() {
            return;
        }
        if self.query.trim().is_empty() && !self.browse_attempted && self.shelf.family().is_some() {
            self.start_search();
        }
        if self.search.is_some() || self.metadata.is_some() || self.focus != Focus::Results {
            return;
        }
        let Some(Pick::Hit(index)) = self.current() else {
            return;
        };
        let hit = &self.hits[index];
        if !self.metadata_attempted.insert(hit.repo.clone()) {
            return;
        }
        let (repo, family) = (hit.repo.clone(), hit.family);
        let (tx, rx) = mpsc::channel();
        self.metadata = Some((repo.clone(), rx));
        std::thread::spawn(move || {
            let _ =
                tx.send(download::metadata(family, &repo).map_err(|error| format!("{error:#}")));
        });
    }

    /// Collects finished background work. Called on every redraw, so results appear without input.
    pub fn poll(&mut self) {
        self.poll_search();
        self.poll_metadata();
        self.poll_pulled();
        if let Some(task) = self.task.take()
            && let Some(task) = self.advance(task)
        {
            self.task = Some(task);
        }
    }

    /// Pasted text goes to the query, like typed characters.
    pub fn paste(&mut self, pasted: &str) {
        self.focus = Focus::Query;
        self.query.push_str(&text::one_line(pasted));
        self.query_changed();
    }

    pub fn key(&mut self, key: &KeyEvent, keys: &Keymap) -> Step {
        if keys.matches(Action::SelectCancel, key) {
            return self.escape();
        }
        if self.picker.is_some() {
            self.picker_key(key, keys);
            return Step::Handled;
        }
        // Typing goes to the query first, so a letter bound to a navigation key still types.
        if let Some(c) = typed_char(key) {
            self.focus = Focus::Query;
            self.query.push(c);
            self.query_changed();
            return Step::Handled;
        }
        if key.code == KeyCode::Backspace {
            if self.focus == Focus::Query {
                self.query.pop();
                self.query_changed();
            }
            return Step::Handled;
        }
        if keys.matches(Action::TabNext, key) || keys.matches(Action::SectionNext, key) {
            let next = self.shelf.next();
            self.switch_shelf(next);
        } else if keys.matches(Action::TabPrev, key) || keys.matches(Action::SectionPrev, key) {
            let prev = self.shelf.prev();
            self.switch_shelf(prev);
        } else if keys.matches(Action::SelectUp, key) {
            self.up();
        } else if keys.matches(Action::SelectDown, key) {
            self.down();
        } else if keys.matches(Action::SelectConfirm, key) {
            return self.confirm();
        } else {
            return Step::Ignored;
        }
        Step::Handled
    }

    /// Key hints for the group's footer (the panel adds its own group hints).
    pub fn footer(&self, keys: &Keymap) -> Vec<String> {
        if self.picker.is_some() {
            return vec![
                format!(
                    "{} choose",
                    pair_label(keys, Action::SelectUp, Action::SelectDown)
                ),
                hint(keys, Action::SelectConfirm, "download"),
                hint(keys, Action::SelectCancel, "back"),
            ];
        }
        let enter = if self.focus == Focus::Query {
            match self.shelf {
                Shelf::Hub(_) => "search",
                Shelf::Embedding => "list",
            }
        } else {
            match self.current() {
                Some(Pick::Installed(_)) => "use as main",
                Some(Pick::Hit(_)) => "list files",
                Some(Pick::Embedding(_)) => "set up",
                Some(Pick::Discovered(index)) if self.discovered[index].available => {
                    "use for embeddings"
                }
                Some(Pick::Discovered(_)) => "",
                None => "",
            }
        };
        let back = if self.is_downloading() {
            "cancel"
        } else if !self.query.is_empty() {
            "clear"
        } else {
            "back"
        };
        let mut hints = vec![
            format!(
                "{} tabs",
                pair_label(keys, Action::SectionPrev, Action::SectionNext)
            ),
            if enter.is_empty() {
                String::new()
            } else {
                hint(keys, Action::SelectConfirm, enter)
            },
            format!(
                "{} move",
                pair_label(keys, Action::SelectUp, Action::SelectDown)
            ),
            hint(keys, Action::SelectCancel, back),
        ];
        hints.retain(|hint| !hint.is_empty());
        hints
    }

    /// The group's right pane: shelves, dashboard facts, the query, then the rows. `height` is the
    /// number of pane rows; the result never exceeds it.
    pub fn body(&self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        // Keep a selectable row visible even when the parent can spare only a few lines.
        // Facts are optional; query and shelf chrome give way before the list does.
        let mut out = Vec::new();
        if height >= 3 {
            out.push(self.shelf_line(width, theme));
        }
        if height >= 4 && self.picker.is_none() {
            out.push(self.facts_line(width, theme));
        }
        if height >= 2 && self.picker.is_none() {
            out.push(self.query_line(width, theme));
        }
        if let Some(picker) = &self.picker {
            if height >= 2 {
                out.push(filled(
                    vec![Span::styled(
                        format!(" Quantizations of {} (smallest first)", picker.repo),
                        theme.fg(Tok::Accent),
                    )],
                    width,
                    Style::default(),
                ));
            }
            let room = height.saturating_sub(out.len());
            let start = window_start(picker.cursor, room);
            for (index, quant) in picker.quants.iter().enumerate().skip(start).take(room) {
                let fill = if index == picker.cursor {
                    selected_fill(theme)
                } else {
                    Style::default()
                };
                out.push(quant_line(quant, theme, width, fill));
            }
            out.truncate(height);
            return out;
        }
        let picks = self.picks();
        let cursor = self.cursor.min(picks.len().saturating_sub(1));
        let rows = self.rows(cursor, width, theme);
        let room = height.saturating_sub(out.len());
        let selected = rows
            .iter()
            .position(|(at, _)| *at == Some(cursor))
            .unwrap_or(0);
        let start = window_start(selected, room);
        out.extend(
            rows.into_iter()
                .skip(start)
                .take(room)
                .map(|(_, line)| line),
        );
        out.truncate(height);
        out
    }

    /// The status line (download progress, the last result or the search status) and the line about the
    /// highlighted row.
    pub fn detail(&self, theme: &Theme) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let first = vec![Span::styled(
            format!(" {}", self.status_text()),
            theme.fg(Tok::Warning),
        )];
        let about = if self.focus == Focus::Query {
            match self.shelf {
                Shelf::Hub(_) => {
                    " Enter searches Hugging Face; an empty query browses suggested models."
                        .to_owned()
                }
                Shelf::Embedding => {
                    " Type to filter the catalog; Enter moves to the list.".to_owned()
                }
            }
        } else {
            match self.current() {
                Some(Pick::Installed(index)) => match self.dashboard.models.get(index) {
                    Some(row) => format!(
                        " speed {} · memory {} · waiting {}",
                        local_models::speed_text(&row.speed),
                        local_models::memory_text(&row.memory),
                        local_models::waiting_text(&row.improvement)
                    ),
                    None => String::new(),
                },
                Some(Pick::Hit(index)) => {
                    let hit = &self.hits[index];
                    let published = hit
                        .created_at
                        .as_deref()
                        .filter(|date| {
                            published_age(Some(date), crate::stats::now_ms() / 1000) != "unknown"
                        })
                        .and_then(|date| date.get(..10))
                        .unwrap_or("unknown");
                    format!(
                        "Repo published {published} · size covers all repository files, not one quantization · Enter lists files"
                    )
                }
                Some(Pick::Embedding(model)) => format!(
                    " {} dims · {} MB · {}",
                    model.dims,
                    model.size_mb,
                    text::one_line(model.description)
                ),
                Some(Pick::Discovered(index)) => {
                    let entry = &self.discovered[index];
                    format!(
                        " {} · {}",
                        entry.selection.provider,
                        text::one_line(&entry.source)
                    )
                }
                None => String::new(),
            }
        };
        (
            first,
            vec![Span::styled(format!(" {about}"), theme.fg(Tok::Muted))],
        )
    }

    // ---- state ----------------------------------------------------------------------------

    fn is_downloading(&self) -> bool {
        matches!(self.task, Some(Task::Download(_)))
    }

    /// Whether the group is idle: no search, listing or download running.
    #[cfg(test)]
    pub fn idle_now(&self) -> bool {
        self.search.is_none() && self.task.is_none()
    }

    #[cfg(test)]
    pub fn query_text(&self) -> &str {
        &self.query
    }

    /// Starts a fake download for tests and returns its cancel flag and the sender of its outcome.
    #[cfg(test)]
    pub fn fake_download(
        &mut self,
        repo: &str,
    ) -> (Arc<AtomicBool>, mpsc::Sender<Result<String, String>>) {
        let (_line_tx, lines) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.task = Some(Task::Download(Download {
            repo: repo.into(),
            family: Family::Mlx,
            expected: 0,
            baseline: 0,
            started: Instant::now(),
            cancel: cancel.clone(),
            lines,
            last: String::new(),
            done,
        }));
        (cancel, done_tx)
    }

    /// Installed models of the current Hugging Face shelf, filtered by the query.
    fn installed_rows(&self) -> Vec<usize> {
        let Some(family) = self.shelf.family() else {
            return Vec::new();
        };
        self.dashboard
            .models
            .iter()
            .enumerate()
            .filter(|(_, row)| row.format == Some(family.label()) && self.query_matches(&row.label))
            .map(|(index, _)| index)
            .collect()
    }

    /// Discovered embedding models matching the query, as indexes into `discovered`.
    fn discovered_rows(&self) -> Vec<usize> {
        if self.shelf != Shelf::Embedding {
            return Vec::new();
        }
        self.discovered
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                self.query_matches(&format!(
                    "{}/{} {}",
                    entry.selection.provider, entry.label, entry.source
                ))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// The curated embedding models, filtered by the query. A model the `ollama` provider already
    /// serves is listed once, as a discovered model.
    fn catalog_rows(&self) -> Vec<&'static embeddings::Model> {
        if self.shelf != Shelf::Embedding {
            return Vec::new();
        }
        embeddings::CATALOG
            .iter()
            .filter(|model| {
                !self.discovered.iter().any(|entry| {
                    entry.selection.provider == "ollama"
                        && embeddings::has_model(
                            std::slice::from_ref(&entry.selection.model),
                            model.name,
                        )
                })
            })
            .filter(|model| self.query_matches(&format!("{} {}", model.name, model.description)))
            .collect()
    }

    /// Editing invalidates a pending response; it must not overwrite a newer query or the
    /// empty-query recommendations. This never starts network work.
    fn query_changed(&mut self) {
        self.cursor = 0;
        self.search = None;
        self.status.clear();
        self.metadata = None;
        self.metadata_attempted.clear();
        self.message.clear();
        if self.shelf.family().is_some() && self.query.trim().is_empty() {
            self.show_curated();
        }
    }

    fn query_matches(&self, haystack: &str) -> bool {
        let needle = self.query.trim().to_lowercase();
        needle.is_empty() || haystack.to_lowercase().contains(&needle)
    }

    /// Already-installed repositories are not offered again as suggested downloads.
    fn available_hits(&self) -> Vec<usize> {
        self.hits
            .iter()
            .enumerate()
            .filter(|(_, hit)| {
                !self
                    .dashboard
                    .models
                    .iter()
                    .any(|row| row.format == Some(hit.family.label()) && row.label == hit.repo)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Every selectable row, in display order: available downloads, then installed models.
    fn picks(&self) -> Vec<Pick> {
        match self.shelf {
            Shelf::Hub(_) => self
                .available_hits()
                .into_iter()
                .map(Pick::Hit)
                .chain(self.installed_rows().into_iter().map(Pick::Installed))
                .collect(),
            Shelf::Embedding => self
                .discovered_rows()
                .into_iter()
                .map(Pick::Discovered)
                .chain(self.catalog_rows().into_iter().map(Pick::Embedding))
                .collect(),
        }
    }

    fn current(&self) -> Option<Pick> {
        let picks = self.picks();
        picks
            .get(self.cursor.min(picks.len().saturating_sub(1)))
            .copied()
    }

    fn up(&mut self) {
        match self.focus {
            Focus::Results if self.cursor == 0 => self.focus = Focus::Query,
            Focus::Results => self.cursor -= 1,
            Focus::Query => {}
        }
    }

    fn down(&mut self) {
        let picks = self.picks();
        match self.focus {
            Focus::Query if !picks.is_empty() => self.focus = Focus::Results,
            Focus::Query => {}
            Focus::Results => {
                let last = picks.len().saturating_sub(1);
                self.cursor = (self.cursor + 1).min(last);
            }
        }
    }

    fn confirm(&mut self) -> Step {
        if self.focus == Focus::Query {
            match self.shelf {
                Shelf::Hub(_) => {
                    self.run_search();
                    if self.query.trim().is_empty() {
                        self.focus = Focus::Results;
                    }
                }
                Shelf::Embedding => self.focus = Focus::Results,
            }
            return Step::Handled;
        }
        match self.current() {
            Some(Pick::Installed(index)) => match self.dashboard.models.get(index) {
                Some(row) => Step::UseModel(row.path.clone()),
                None => Step::Handled,
            },
            Some(Pick::Hit(index)) => {
                self.activate(index);
                Step::Handled
            }
            Some(Pick::Embedding(model)) => {
                self.message = format!(
                    "Setting up {} in the background: Ollama pulls it, then Dial assigns it to memory.",
                    model.name
                );
                Step::SetupEmbedding(model.name.to_owned())
            }
            Some(Pick::Discovered(index)) => {
                let entry = &self.discovered[index];
                if entry.available {
                    Step::UseEmbedding(entry.selection.clone())
                } else {
                    self.message = format!(
                        "{} cannot be assigned yet: {}",
                        entry.label,
                        text::one_line(&entry.source)
                    );
                    Step::Handled
                }
            }
            None => Step::Handled,
        }
    }

    /// Esc: cancel a running download, else close the picker or clear the query, else leave the group.
    fn escape(&mut self) -> Step {
        if let Some(Task::Download(download)) = &self.task {
            if !download.cancel.swap(true, Ordering::SeqCst) {
                self.message = format!("Cancelling the download of {}…", download.repo);
            }
            return Step::Handled;
        }
        if let Some(Task::Inspect { repo, .. }) = self.task.take() {
            self.message = format!("Stopped listing {repo}.");
            return Step::Handled;
        }
        if self.picker.take().is_some() {
            return Step::Handled;
        }
        if !self.query.is_empty() {
            self.query.clear();
            self.query_changed();
            self.focus = Focus::Results;
            return Step::Handled;
        }
        Step::Back
    }

    fn picker_key(&mut self, key: &KeyEvent, keys: &Keymap) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        if keys.matches(Action::SelectUp, key) {
            picker.cursor = picker.cursor.saturating_sub(1);
            return;
        }
        if keys.matches(Action::SelectDown, key) {
            picker.cursor = (picker.cursor + 1).min(picker.quants.len().saturating_sub(1));
            return;
        }
        if !keys.matches(Action::SelectConfirm, key) {
            return;
        }
        let Some(quant) = picker.quants.get(picker.cursor).cloned() else {
            return;
        };
        let repo = picker.repo.clone();
        self.picker = None;
        self.begin_download(repo, Choice::Files(quant.files), quant.bytes, Family::Gguf);
    }

    /// Moves to another shelf. Hugging Face shelves show the query's results (or the curated list);
    /// the embedding shelf reads Ollama's list.
    fn switch_shelf(&mut self, shelf: Shelf) {
        self.shelf = shelf;
        self.picker = None;
        self.cursor = 0;
        self.status.clear();
        match shelf {
            Shelf::Hub(_) if self.query.trim().is_empty() => {
                self.show_curated();
                self.focus = Focus::Results;
            }
            Shelf::Hub(_) => self.run_search(),
            Shelf::Embedding => {
                self.search = None;
                self.focus = Focus::Results;
                self.refresh_pulled();
            }
        }
    }

    fn show_curated(&mut self) {
        if let Some(family) = self.shelf.family() {
            self.hits = download::recommended(family);
        }
        self.curated = true;
        self.cursor = 0;
        self.search = None;
        self.browse_attempted = false;
        self.metadata = None;
        self.metadata_attempted.clear();
        self.status.clear();
    }

    /// Empty searches retain offline suggestions while refreshing the live catalog.
    fn run_search(&mut self) {
        if self.query.trim().is_empty() {
            self.show_curated();
            if self.source.is_none() {
                return;
            }
        } else {
            self.hits.clear();
            self.curated = false;
            self.cursor = 0;
        }
        self.start_search();
    }

    fn start_search(&mut self) {
        let Some(family) = self.shelf.family() else {
            return;
        };
        let query = self.query.trim().to_owned();
        self.browse_attempted = true;
        self.metadata = None;
        self.metadata_attempted.clear();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(download::search(family, &query).map_err(|error| format!("{error:#}")));
        });
        self.search = Some(rx);
        self.status = format!("loading {} catalog from Hugging Face…", family.label());
    }

    /// Enter on a result: list its files (sizes, and quantizations for GGUF). Nothing is downloaded yet.
    fn activate(&mut self, index: usize) {
        if self.task.is_some() {
            self.message = "A download is already running; Esc cancels it.".into();
            return;
        }
        if self.source.is_none() {
            self.message = "Downloads are unavailable in this view.".into();
            return;
        }
        let Some(hit) = self.hits.get(index).cloned() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let listed = hit.repo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(download::tree(&listed).map_err(|error| format!("{error:#}")));
        });
        self.message = format!("Listing the files of {}…", hit.repo);
        self.task = Some(Task::Inspect {
            repo: hit.repo,
            family: hit.family,
            rx,
        });
    }

    /// Reads the model list of the Ollama endpoint setup uses (a configured `providers.ollama`, or
    /// the local default) on a worker thread. Nothing is pulled. Without a layout nothing is listed.
    fn refresh_pulled(&mut self) {
        let Some(source) = self.source.as_ref() else {
            self.pulled = None;
            self.pulled_rx = None;
            return;
        };
        let layout = source.layout.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx
                .send(embeddings::configured_models(&layout).map_err(|error| format!("{error:#}")));
        });
        self.pulled_rx = Some(rx);
    }

    /// Starts a download unless one is already running.
    fn begin_download(&mut self, repo: String, choice: Choice, expected: u64, family: Family) {
        if self.task.is_some() {
            self.message = "A download is already running; Esc cancels it.".into();
            return;
        }
        self.start_download(repo, choice, expected, family);
    }

    fn start_download(&mut self, repo: String, choice: Choice, expected: u64, family: Family) {
        let Some(source) = self.source.as_ref() else {
            self.message = "Downloads are unavailable in this view.".into();
            return;
        };
        let python = source.python.clone();
        let (line_tx, lines) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker_repo = repo.clone();
        std::thread::spawn(move || {
            let progress = |line: &str| {
                let _ = line_tx.send(line.to_owned());
            };
            let result = download::ensure_local_model_with(
                &python,
                &worker_repo,
                &choice,
                &worker_cancel,
                &progress,
            );
            let _ = done_tx.send(result.map_err(|error| format!("{error:#}")));
        });
        let baseline = download::cached_bytes(&repo);
        self.message.clear();
        self.task = Some(Task::Download(Download {
            repo,
            family,
            expected,
            baseline,
            started: Instant::now(),
            cancel,
            lines,
            last: "starting".into(),
            done,
        }));
    }

    fn poll_search(&mut self) {
        let outcome = match &self.search {
            Some(rx) => rx.try_recv(),
            None => Err(TryRecvError::Empty),
        };
        match outcome {
            Ok(Ok(hits)) => {
                let selected = self.current();
                let selected_hit = match selected {
                    Some(Pick::Hit(index)) => self.hits.get(index).cloned(),
                    _ => None,
                };
                // A background browse must not replace the model under Enter while navigating.
                let mut hits = hits;
                if self.query.trim().is_empty() {
                    if let Some(hit) = &selected_hit {
                        if !hits.iter().any(|new| new.repo == hit.repo) {
                            hits.insert(0, hit.clone());
                        }
                    }
                }
                if !hits.is_empty() || !self.query.trim().is_empty() {
                    self.hits = hits;
                    self.curated = false;
                }
                self.cursor = self
                    .picks()
                    .iter()
                    .position(|pick| match (pick, selected) {
                        (Pick::Hit(index), Some(Pick::Hit(_))) => selected_hit
                            .as_ref()
                            .is_some_and(|hit| self.hits[*index].repo == hit.repo),
                        (Pick::Installed(index), Some(Pick::Installed(old))) => *index == old,
                        _ => false,
                    })
                    .unwrap_or(0);
                self.status = format!("{} available models", self.available_hits().len());
                self.search = None;
                if !self.query.trim().is_empty() && !self.picks().is_empty() {
                    self.focus = Focus::Results;
                }
            }
            Ok(Err(error)) => {
                self.status = error;
                self.search = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.status = "the search stopped unexpectedly".into();
                self.search = None;
            }
        }
    }

    fn poll_metadata(&mut self) {
        let Some((repo, rx)) = &self.metadata else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(details)) => {
                if let Some(hit) = self
                    .hits
                    .iter_mut()
                    .find(|hit| hit.repo == *repo && hit.family == details.family)
                {
                    hit.created_at = details.created_at.or(hit.created_at.take());
                    hit.size_bytes = details.size_bytes.or(hit.size_bytes);
                    hit.parameters = details.parameters.or(hit.parameters);
                    hit.downloads = details.downloads.or(hit.downloads);
                    hit.likes = details.likes.or(hit.likes);
                }
                self.metadata = None;
            }
            Ok(Err(error)) => {
                self.status = format!("Model details unavailable: {error}");
                self.metadata = None;
            }
            Err(TryRecvError::Disconnected) => {
                self.status = "the model details request stopped unexpectedly".into();
                self.metadata = None;
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    fn poll_pulled(&mut self) {
        let outcome = match &self.pulled_rx {
            Some(rx) => rx.try_recv(),
            None => return,
        };
        match outcome {
            Ok(Ok(names)) => {
                self.pulled = Some(names);
                self.pulled_rx = None;
            }
            Ok(Err(error)) => {
                self.pulled = None;
                self.pulled_rx = None;
                if self.shelf == Shelf::Embedding {
                    self.message = format!("Ollama's model list is unavailable: {error}");
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.pulled_rx = None,
        }
    }

    /// Advances one task; returns it again while it is still running.
    fn advance(&mut self, task: Task) -> Option<Task> {
        match task {
            Task::Inspect { repo, family, rx } => match rx.try_recv() {
                Ok(Ok(files)) => {
                    self.finish_inspect(repo, family, &files);
                    None
                }
                Ok(Err(error)) => {
                    self.message = format!("Could not list {repo}: {error}");
                    None
                }
                Err(TryRecvError::Empty) => Some(Task::Inspect { repo, family, rx }),
                Err(TryRecvError::Disconnected) => {
                    self.message = format!("Listing {repo} stopped unexpectedly.");
                    None
                }
            },
            Task::Download(mut download) => {
                while let Ok(line) = download.lines.try_recv() {
                    download.last = line;
                }
                match download.done.try_recv() {
                    Ok(Ok(path)) => {
                        self.finish_download(&download.repo, download.family, &path);
                        None
                    }
                    Ok(Err(error)) => {
                        self.message = if download.cancel.load(Ordering::SeqCst) {
                            format!("Cancelled the download of {}.", download.repo)
                        } else {
                            format!("Download of {} failed: {error}", download.repo)
                        };
                        None
                    }
                    Err(TryRecvError::Empty) => Some(Task::Download(download)),
                    Err(TryRecvError::Disconnected) => {
                        self.message =
                            format!("The download of {} stopped unexpectedly.", download.repo);
                        None
                    }
                }
            }
        }
    }

    /// MLX repositories download straight away; GGUF repositories open the quantization picker first.
    fn finish_inspect(&mut self, repo: String, family: Family, files: &[(String, u64)]) {
        match family {
            Family::Gguf => {
                let quants = download::quant_options(files);
                if quants.is_empty() {
                    self.message = format!("{repo} has no GGUF weights to download.");
                } else {
                    self.message.clear();
                    self.picker = Some(Picker {
                        repo,
                        quants,
                        cursor: 0,
                    });
                }
            }
            Family::Mlx => {
                let expected = download::snapshot_bytes(files);
                self.begin_download(repo, Choice::Auto, expected, family);
            }
        }
    }

    /// Refreshes the installed list, shows the new model on its shelf and says how to make it the main
    /// one. Asks for a rescan so the models list sees it too.
    fn finish_download(&mut self, repo: &str, family: Family, path: &str) {
        if let Some(source) = &self.source {
            self.dashboard =
                local_models::collect(&source.layout, &source.settings, crate::stats::now_ms());
        }
        self.rescan = true;
        self.shelf = Shelf::Hub(family);
        self.picker = None;
        self.focus = Focus::Results;
        self.cursor = self
            .picks()
            .iter()
            .position(|pick| match pick {
                Pick::Installed(index) => self
                    .dashboard
                    .models
                    .get(*index)
                    .is_some_and(|row| same_model(&row.path, path)),
                _ => false,
            })
            .unwrap_or(0);
        self.message = format!("Downloaded {repo}. Enter makes it the main model.");
    }

    // ---- drawing --------------------------------------------------------------------------

    fn status_text(&self) -> String {
        match &self.task {
            Some(Task::Download(download)) => {
                let done = download::cached_bytes(&download.repo).saturating_sub(download.baseline);
                let progress = if download.expected > 0 {
                    format!(
                        "{} of {}",
                        human_bytes(done),
                        human_bytes(download.expected)
                    )
                } else {
                    human_bytes(done)
                };
                let state = if download.cancel.load(Ordering::SeqCst) {
                    "cancelling"
                } else {
                    "downloading"
                };
                format!(
                    "{state} {}: {progress} · {}s · {}",
                    download.repo,
                    download.started.elapsed().as_secs(),
                    download.last
                )
            }
            _ if !self.message.is_empty() => self.message.clone(),
            _ if self.shelf.family().is_some() => self.status.clone(),
            _ => String::new(),
        }
    }

    fn shelf_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        let mut spans = vec![Span::raw(" ")];
        for shelf in SHELVES {
            let style = if shelf == self.shelf {
                theme
                    .fg(Tok::Accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                theme.fg(Tok::Muted)
            };
            spans.push(Span::styled(format!(" {} ", shelf.label()), style));
        }
        Line::from(fit(spans, width))
    }

    fn facts_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        let facts = match self.shelf {
            Shelf::Hub(family) => {
                let memory = match self.dashboard.memory_bytes {
                    Some(bytes) => format!(
                        "{:.1} GiB {}",
                        bytes as f64 / (1u64 << 30) as f64,
                        self.dashboard.memory_source
                    ),
                    None => "unknown".into(),
                };
                let installed = self
                    .dashboard
                    .models
                    .iter()
                    .filter(|row| row.format == Some(family.label()))
                    .count();
                format!(
                    " {installed} installed · memory {memory} · stats {}",
                    self.dashboard.stats
                )
            }
            Shelf::Embedding => match &self.pulled {
                Some(_) => " Discovered embedding models first; Enter assigns one to the embedding role. Curated Ollama models below are pulled first.".to_owned(),
                None => " Discovered embedding models first; Enter assigns one. Ollama's model list is not read yet; curated setup starts Ollama if needed.".to_owned(),
            },
        };
        Line::from(fit(
            vec![Span::styled(text::one_line(&facts), theme.fg(Tok::Muted))],
            width,
        ))
    }

    fn query_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        let focused = self.focus == Focus::Query;
        let mut spans = vec![Span::styled(
            format!(" / {}{}", self.query, if focused { "_" } else { "" }),
            theme.fg(if focused { Tok::Accent } else { Tok::Text }),
        )];
        if focused && self.query.is_empty() {
            let placeholder = match self.shelf {
                Shelf::Hub(_) => "  type a name, Enter searches Hugging Face",
                Shelf::Embedding => "  type to filter the catalog",
            };
            spans.push(Span::styled(placeholder, theme.fg(Tok::Dim)));
        }
        Line::from(fit(spans, width))
    }

    /// The list rows with their pick index (`None` for headings and notes), in display order.
    fn rows(
        &self,
        cursor: usize,
        width: usize,
        theme: &Theme,
    ) -> Vec<(Option<usize>, Line<'static>)> {
        let heading = |label: &str| {
            (
                None,
                filled(
                    vec![Span::styled(format!(" {label}"), theme.fg(Tok::Muted))],
                    width,
                    Style::default(),
                ),
            )
        };
        let selected = |at: usize| self.focus == Focus::Results && at == cursor;
        let fill = |at: usize| {
            if selected(at) {
                selected_fill(theme)
            } else {
                Style::default()
            }
        };
        let mut rows = Vec::new();
        match self.shelf {
            Shelf::Hub(family) => {
                let available = self.available_hits();
                rows.push(heading(if available.is_empty() {
                    "No new models match · try a search"
                } else if self.curated {
                    "Suggested downloads · available without a search"
                } else if self.query.trim().is_empty() {
                    "Suggested downloads · popular on Hugging Face"
                } else {
                    "Available downloads · most downloaded first"
                }));
                let mut at = 0;
                for index in available {
                    let hit = &self.hits[index];
                    rows.push((
                        Some(at),
                        filled(hit_spans(hit, theme, width), width, fill(at)),
                    ));
                    rows.push((
                        None,
                        filled(
                            vec![Span::styled(
                                format!("   {}", hit_metadata(hit, crate::stats::now_ms() / 1000)),
                                theme.fg(Tok::Muted),
                            )],
                            width,
                            fill(at),
                        ),
                    ));
                    at += 1;
                }
                let installed = self.installed_rows();
                if !installed.is_empty() {
                    rows.push(heading("Installed on this machine"));
                } else if self.query.trim().is_empty() {
                    rows.push(heading(&format!(
                        "None downloaded in {} yet.",
                        family.label()
                    )));
                }
                for index in installed {
                    if let Some(row) = self.dashboard.models.get(index) {
                        rows.push((
                            Some(at),
                            filled(list_spans(row, theme, width), width, fill(at)),
                        ));
                    }
                    at += 1;
                }
            }
            Shelf::Embedding => {
                let discovered = self.discovered_rows();
                let mut at = 0;
                if !discovered.is_empty() {
                    rows.push(heading(
                        "Discovered embedding models · Enter assigns the embedding role",
                    ));
                }
                for index in discovered {
                    rows.push((
                        Some(at),
                        filled(
                            discovered_spans(&self.discovered[index], theme, width),
                            width,
                            fill(at),
                        ),
                    ));
                    at += 1;
                }
                rows.push(heading(
                    "Curated Ollama embedding models · Enter sets one up",
                ));
                let models = self.catalog_rows();
                if models.is_empty() {
                    rows.push(heading("No curated embedding model matches the query."));
                }
                for model in models {
                    let pulled = self
                        .pulled
                        .as_ref()
                        .map(|names| embeddings::has_model(names, model.name));
                    rows.push((
                        Some(at),
                        filled(
                            embedding_spans(model, pulled, theme, width),
                            width,
                            fill(at),
                        ),
                    ));
                    at += 1;
                }
            }
        }
        rows
    }
}

/// Stops a running download when the group is dropped (closing `/models`).
impl Drop for ModelDownloads {
    fn drop(&mut self) {
        if let Some(Task::Download(download)) = &self.task {
            download.cancel.store(true, Ordering::SeqCst);
        }
    }
}

fn quant_line(quant: &Quant, theme: &Theme, width: usize, fill: Style) -> Line<'static> {
    let label = text::pad(
        &text::ellipsize(&text::one_line(&quant.label), 28, theme.ascii()),
        28,
    );
    let files = quant.files.len();
    filled(
        vec![Span::styled(
            format!(
                " {label} {:>9}  {files} file{}",
                human_bytes(quant.bytes),
                if files == 1 { "" } else { "s" }
            ),
            theme.fg(Tok::Text),
        )],
        width,
        fill,
    )
}

/// The first row to show so that `cursor` stays inside a window of `height` rows.
fn window_start(cursor: usize, height: usize) -> usize {
    if cursor < height {
        0
    } else {
        cursor + 1 - height
    }
}

/// Whether the installed model at `listed` is the one a download produced (`downloaded` may be a file
/// inside that directory, as GGUF downloads are).
fn same_model(listed: &str, downloaded: &str) -> bool {
    let canonical =
        |path: &str| std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let (listed, downloaded) = (canonical(listed), canonical(downloaded));
    downloaded == listed || downloaded.starts_with(&listed)
}

/// `1.5 GiB`, `312 MiB`, `8 KiB` or `12 B`.
fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Hugging Face counts as `1.2M`, `34.5k` or `812`; unknown counts show as `n/a`.
fn compact(count: Option<u64>) -> String {
    match count {
        None => "n/a".into(),
        Some(n) if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        Some(n) if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        Some(n) => n.to_string(),
    }
}

/// Model names get their own row so metadata never crowds them out.
fn hit_spans(hit: &Hit, theme: &Theme, width: usize) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!(
            " {}",
            text::ellipsize(
                &text::one_line(&hit.repo),
                width.saturating_sub(1),
                theme.ascii(),
            )
        ),
        theme.fg(Tok::Text),
    )]
}

fn hit_metadata(hit: &Hit, now_secs: i64) -> String {
    let params = hit
        .parameters
        .map(|n| {
            if n >= 1_000_000_000 {
                format!("{:.1}B", n as f64 / 1e9)
            } else {
                format!("{:.0}M", n as f64 / 1e6)
            }
        })
        .or_else(|| download::params_hint(&hit.repo))
        .unwrap_or_else(|| "params unknown".into());
    let size = hit
        .size_bytes
        .map(human_bytes)
        .unwrap_or_else(|| "unknown".into());
    format!(
        "{params} · repo {size} · published {} · {} downloads · {} likes",
        published_age(hit.created_at.as_deref(), now_secs),
        compact(hit.downloads),
        compact(hit.likes)
    )
}

/// HF dates are UTC ISO timestamps. Reuse the platform's calendar validation and reject
/// unknown offsets, malformed fractions and future dates instead of inventing an age.
fn published_age(date: Option<&str>, now_secs: i64) -> String {
    let parsed = date.and_then(|date| {
        let date = date.strip_suffix('Z')?;
        let base = if let Some((base, fraction)) = date.split_once('.') {
            if fraction.is_empty() || !fraction.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            base
        } else {
            date
        };
        crate::platform::time::parse_utc(base)
    });
    let Some(age) = parsed
        .and_then(|time| now_secs.checked_sub(time))
        .filter(|age| *age >= 0)
    else {
        return "unknown".into();
    };
    let days = age / 86_400;
    match days {
        0 => "today".into(),
        1..=29 => format!("{days}d ago"),
        30..=364 => format!("{}mo ago", days / 30),
        _ => format!("{}y ago", days / 365),
    }
}

/// One curated embedding row: the model (ellipsized first), whether Ollama has it, then its size.
fn embedding_spans(
    model: &embeddings::Model,
    pulled: Option<bool>,
    theme: &Theme,
    width: usize,
) -> Vec<Span<'static>> {
    const STATE_W: usize = 9;
    let stats = format!("{:>4} dims {:>5} MB", model.dims, model.size_mb);
    let name_w = width
        .saturating_sub(1 + STATE_W + 1 + text::width(&stats) + 1)
        .max(8);
    let name = text::pad(&text::ellipsize(model.name, name_w, theme.ascii()), name_w);
    let (state, tok) = match pulled {
        Some(true) => ("pulled", Tok::Success),
        Some(false) => ("", Tok::Muted),
        None => ("unknown", Tok::Dim),
    };
    vec![
        Span::styled(format!(" {name} "), theme.fg(Tok::Text)),
        Span::styled(text::pad(state, STATE_W), theme.fg(tok)),
        Span::styled(stats, theme.fg(Tok::Muted)),
    ]
}

/// One discovered embedding row: `provider/model`, whether it is served, then its source.
fn discovered_spans(entry: &ModelEntry, theme: &Theme, width: usize) -> Vec<Span<'static>> {
    const STATE_W: usize = 11;
    let name_w = (width.saturating_sub(1 + STATE_W + 1) * 3 / 5).max(8);
    let name = text::one_line(&format!("{}/{}", entry.selection.provider, entry.label));
    let name = text::pad(&text::ellipsize(&name, name_w, theme.ascii()), name_w);
    let (state, tok) = if entry.available {
        ("ready", Tok::Success)
    } else {
        ("not served", Tok::Warning)
    };
    vec![
        Span::styled(format!(" {name} "), theme.fg(Tok::Text)),
        Span::styled(text::pad(state, STATE_W), theme.fg(tok)),
        Span::styled(text::one_line(&entry.source), theme.fg(Tok::Muted)),
    ]
}

/// One installed-model row: `label  [format]  median tok/s  fit  first waiting item`. The label gives way
/// first (ellipsized); the format, speed and fit columns keep fixed widths so the verdict survives narrow
/// panels, and the waiting item is what gets clipped last-resort at the far right.
fn list_spans(row: &ModelRow, theme: &Theme, width: usize) -> Vec<Span<'static>> {
    const FORMAT_W: usize = 7;
    const SPEED_W: usize = 13;
    const FIT_W: usize = 13;
    let speed = match row.speed.median_tok_per_s {
        Some(median) => format!("{median:.1} tok/s"),
        None => "tok/s unknown".into(),
    };
    let fit_label = match row.memory.fit {
        Fit::Fits => "fits",
        Fit::Tight => "tight",
        Fit::WontFit => "will not fit",
        Fit::Unknown => "fit unknown",
    };
    let waiting = match &row.improvement.waiting {
        None => "unknown".to_owned(),
        Some(items) => items.first().cloned().unwrap_or_else(|| "ready".into()),
    };
    // Cells before the label's column end: margin, label gap, format, speed and fit columns with their gaps.
    let label_w = width
        .saturating_sub(1 + 1 + FORMAT_W + 1 + SPEED_W + 1 + FIT_W + 1)
        .max(1);
    let label = text::pad(
        &text::ellipsize(&text::one_line(&row.label), label_w, theme.ascii()),
        label_w,
    );
    let format = text::pad(&format!("[{}]", row.format.unwrap_or("?")), FORMAT_W);
    vec![Span::styled(
        format!(
            " {label} {format} {} {} {waiting}",
            text::pad(&speed, SPEED_W),
            text::pad(fit_label, FIT_W)
        ),
        theme.fg(Tok::Text),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::local_models::{Improvement, Memory};
    use crate::stats::DecodeSpeed;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(label: &str) -> ModelRow {
        ModelRow {
            label: label.into(),
            path: format!("/models/{label}"),
            source: "test".into(),
            format: Some("MLX"),
            speed: DecodeSpeed::default(),
            memory: Memory {
                weights_bytes: None,
                resident_bytes: None,
                context_tokens: None,
                fit: Fit::Unknown,
            },
            improvement: Improvement {
                store: "no_store",
                store_dir: None,
                version: None,
                kind: None,
                adapter: None,
                last_gate: None,
                verified_turns: None,
                lora_min_examples: 24,
                new_turns: None,
                min_new_turns: 5,
                cooldown_secs: None,
                waiting: Some(vec!["no recorded turns for this model yet".into()]),
            },
        }
    }

    fn dashboard(models: Vec<ModelRow>) -> Dashboard {
        Dashboard {
            generated_ms: 0,
            memory_bytes: None,
            memory_source: "unknown",
            stats: "missing",
            models,
        }
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn selection_pane(models: Vec<ModelRow>) -> ModelDownloads {
        let mut group = ModelDownloads::fixed(dashboard(models));
        group.focus = Focus::Results;
        group
    }

    #[test]
    fn arrows_move_the_selection_within_the_list_and_escape_backs_out() {
        let keys = Keymap::defaults();
        let mut group = selection_pane(vec![row("a"), row("b")]);
        group.key(&key(KeyCode::Down), &keys);
        group.key(&key(KeyCode::Down), &keys);
        assert_eq!(group.cursor, 2, "suggestions precede installed models");
        assert!(matches!(group.current(), Some(Pick::Hit(2))));
        let last = group.picks().len() - 1;
        for _ in 0..=last {
            group.key(&key(KeyCode::Down), &keys);
        }
        assert_eq!(group.cursor, last, "the cursor stops at the last result");
        for _ in 1..last {
            group.key(&key(KeyCode::Up), &keys);
        }
        assert_eq!(group.cursor, 1);
        assert!(matches!(group.current(), Some(Pick::Hit(1))));
        group.key(&key(KeyCode::Up), &keys);
        assert_eq!(group.cursor, 0);
        assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Back);
        group.paste("qwen");
        assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Handled);
        assert!(group.query.is_empty(), "Esc clears a query first");
        assert_eq!(group.focus, Focus::Results);
        assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Back);
    }

    #[test]
    fn enter_on_an_installed_model_asks_for_it_as_the_main_model() {
        let keys = Keymap::defaults();
        let mut group = selection_pane(vec![row("a"), row("b")]);
        group.cursor = group
            .picks()
            .iter()
            .position(|pick| matches!(pick, Pick::Installed(1)))
            .unwrap();
        assert_eq!(
            group.key(&key(KeyCode::Enter), &keys),
            Step::UseModel("/models/b".into())
        );
        let mut empty = ModelDownloads::fixed(dashboard(Vec::new()));
        empty.focus = Focus::Results;
        assert_eq!(empty.key(&key(KeyCode::Enter), &keys), Step::Handled);
    }

    #[test]
    fn typing_edits_the_query_and_never_starts_a_search_or_download() {
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
        assert!(group.curated && !group.hits.is_empty());
        for c in "qwen".chars() {
            group.key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &keys);
        }
        group.key(&key(KeyCode::Backspace), &keys);
        assert_eq!(group.query_text(), "qwe");
        assert!(group.idle_now(), "typing never starts a search or download");
    }

    #[test]
    fn an_empty_query_shows_the_curated_list_for_the_chosen_format_without_network() {
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.key(&key(KeyCode::Up), &keys);
        group.key(&key(KeyCode::Enter), &keys);
        assert!(group.search.is_none());
        assert!(group.curated);
        group.key(&key(KeyCode::Tab), &keys);
        assert_eq!(group.shelf, Shelf::Hub(Family::Gguf));
        assert!(group.hits.iter().all(|hit| hit.family == Family::Gguf));
    }

    #[test]
    fn opening_selects_a_visible_row_without_searching() {
        let keys = Keymap::defaults();
        let group = ModelDownloads::idle();
        assert_eq!(group.focus, Focus::Results);
        assert!(matches!(group.current(), Some(Pick::Hit(0))));
        assert!(group.idle_now());
        let shown = text_of(&group.body(100, 8, &Theme::dark()));
        assert!(shown.contains(&group.hits[0].repo), "{shown}");

        let mut installed = ModelDownloads::fixed(dashboard(vec![row("ready")]));
        assert!(matches!(installed.current(), Some(Pick::Hit(_))));
        installed.cursor = installed
            .picks()
            .iter()
            .position(|pick| matches!(pick, Pick::Installed(0)))
            .unwrap();
        assert_eq!(
            installed.key(&key(KeyCode::Enter), &keys),
            Step::UseModel("/models/ready".into())
        );
    }

    #[test]
    fn short_panes_keep_the_first_model_visible_without_a_query() {
        let group = ModelDownloads::fixed(dashboard(Vec::new()));
        for height in 1..=5 {
            let lines = group.body(100, height, &Theme::dark());
            let shown = text_of(&lines);
            assert!(lines.len() <= height);
            assert!(
                shown.contains(&group.hits[0].repo),
                "height {height}: {shown}"
            );
        }
    }

    #[test]
    fn clearing_query_restores_recommendations_and_ignores_old_responses() {
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.paste("x");
        let (tx, rx) = mpsc::channel();
        group.search = Some(rx);
        group.hits.clear();
        group.curated = false;
        tx.send(Ok(Vec::new())).unwrap();
        group.key(&key(KeyCode::Backspace), &keys);
        group.poll();
        assert_eq!(group.query_text(), "");
        assert!(group.curated);
        assert_eq!(group.hits, download::recommended(Family::Mlx));
        assert!(group.idle_now());
    }

    #[test]
    fn completed_search_focuses_results_so_enter_selects_instead_of_searching_again() {
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.paste("qwen");
        let (tx, rx) = mpsc::channel();
        group.search = Some(rx);
        tx.send(Ok(download::recommended(Family::Mlx))).unwrap();
        group.poll();
        assert_eq!(group.focus, Focus::Results);
        assert!(matches!(group.current(), Some(Pick::Hit(0))));
        group.key(&key(KeyCode::Enter), &keys);
        assert!(group.message.contains("Downloads are unavailable"));
        assert!(group.idle_now(), "Enter must not start another search");
    }

    #[test]
    fn enter_on_an_embedding_row_asks_for_its_setup() {
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.shelf = Shelf::Embedding;
        group.focus = Focus::Results;
        let first = embeddings::CATALOG[0].name;
        assert_eq!(
            group.key(&key(KeyCode::Enter), &keys),
            Step::SetupEmbedding(first.into())
        );
        assert_eq!(embeddings::setup_id(first), format!("embeddings:{first}"));
    }

    fn embedder(provider: &str, model: &str, available: bool) -> ModelEntry {
        ModelEntry {
            selection: ModelSelection::new(provider, model),
            label: model.into(),
            source: provider.into(),
            available,
            metadata: crate::llm::catalog::ModelMetadata {
                kind: crate::llm::catalog::ModelKind::Embedding,
                ..Default::default()
            },
        }
    }

    #[test]
    fn discovered_embedding_models_lead_the_shelf_and_enter_assigns_them() {
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.shelf = Shelf::Embedding;
        group.focus = Focus::Results;
        let mut chat = embedder("ollama", "llama3:latest", true);
        chat.metadata.kind = crate::llm::catalog::ModelKind::Chat;
        group.set_embedding_models(&[
            embedder("ollama", "embeddinggemma:latest", true),
            chat,
            embedder("lm-studio", "text-embedding-qwen3", true),
            embedder("mlx", "/models/bge", false),
        ]);
        let shown = text_of(&group.body(120, 30, &Theme::dark()));
        assert!(shown.contains("ollama/embeddinggemma:latest"), "{shown}");
        assert!(shown.contains("lm-studio/text-embedding-qwen3"), "{shown}");
        assert!(shown.contains("not served"), "{shown}");
        assert!(!shown.contains("llama3"), "chat models stay off the shelf");
        assert!(
            !group.catalog_rows().iter().any(|m| m.name == "embeddinggemma"),
            "a curated model already served by Ollama is listed once"
        );
        assert_eq!(
            group.key(&key(KeyCode::Enter), &keys),
            Step::UseEmbedding(ModelSelection::new("ollama", "embeddinggemma:latest"))
        );
        group.cursor = 2;
        assert_eq!(group.key(&key(KeyCode::Enter), &keys), Step::Handled);
        assert!(group.message.contains("cannot be assigned yet"));
        group.cursor = 3;
        assert!(matches!(group.current(), Some(Pick::Embedding(_))));
    }

    #[test]
    fn escape_cancels_a_running_download_before_it_closes_the_panel() {
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
        let (cancel, _done) = group.fake_download("org/model");
        assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Handled);
        assert!(cancel.load(Ordering::SeqCst), "Esc sets the cancel flag");
        assert!(group.message.contains("Cancelling"), "{}", group.message);
        assert!(
            group.task.is_some(),
            "the worker reports the outcome itself"
        );
        group.task = None;
        assert_eq!(group.key(&key(KeyCode::Esc), &keys), Step::Back);
    }

    #[test]
    fn a_finished_download_reports_its_result_and_keeps_the_group_usable() {
        let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
        let (_cancel, done) = group.fake_download("org/model");
        done.send(Err("download cancelled".to_owned())).unwrap();
        group.poll();
        assert!(group.task.is_none());
        assert!(group.message.contains("failed"), "{}", group.message);
        assert!(
            group.message.contains("download cancelled"),
            "{}",
            group.message
        );
        assert!(!group.take_rescan());
    }

    #[test]
    fn a_finished_download_asks_for_a_rescan_once() {
        let mut group = ModelDownloads::fixed(dashboard(vec![row("a")]));
        let (_cancel, done) = group.fake_download("org/model");
        done.send(Ok("/no/such/path".to_owned())).unwrap();
        group.poll();
        assert!(group.take_rescan());
        assert!(!group.take_rescan());
    }

    #[test]
    fn same_model_accepts_a_file_inside_the_listed_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("snapshot");
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("m-Q4.gguf");
        std::fs::write(&file, b"x").unwrap();
        let other = temp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(same_model(&dir.to_string_lossy(), &file.to_string_lossy()));
        assert!(same_model(&dir.to_string_lossy(), &dir.to_string_lossy()));
        assert!(!same_model(
            &other.to_string_lossy(),
            &file.to_string_lossy()
        ));
    }

    #[test]
    fn unknown_sources_render_as_unknown_not_as_numbers() {
        let mut group = selection_pane(vec![row("mlx-model")]);
        group.cursor = group
            .picks()
            .iter()
            .position(|pick| matches!(pick, Pick::Installed(0)))
            .unwrap();
        let theme = Theme::dark();
        let shown = text_of(&group.body(120, 30, &theme));
        assert!(shown.contains("stats missing"), "{shown}");
        let line = shown
            .lines()
            .find(|line| line.contains("mlx-model"))
            .unwrap_or_default();
        assert!(
            line.contains("[MLX]")
                && line.contains("tok/s unknown")
                && line.contains("fit unknown"),
            "{shown}"
        );
        let (_, about) = group.detail(&theme);
        let about = text_of(&[Line::from(about)]);
        assert!(about.contains("unknown (stats unavailable)"), "{about}");
        assert!(
            about.contains("no recorded turns for this model yet"),
            "{about}"
        );
    }

    #[test]
    fn an_empty_or_tiny_dashboard_draws_without_panicking() {
        let theme = Theme::dark();
        let empty = ModelDownloads::fixed(dashboard(Vec::new()));
        assert!(text_of(&empty.body(80, 24, &theme)).contains("None downloaded in MLX yet"));
        let tiny = ModelDownloads::fixed(dashboard(vec![row("a"), row("b"), row("c")]));
        assert!(!tiny.body(40, 4, &theme).is_empty());
    }

    #[test]
    fn the_list_keeps_a_row_on_short_panels() {
        let theme = Theme::dark();
        let group = selection_pane(vec![row("alpha"), row("beta")]);
        for height in [4, 8, 12, 13] {
            let lines = group.body(80, height, &theme);
            assert!(
                text_of(&lines).contains(&group.hits[0].repo),
                "height {height}"
            );
            assert!(lines.len() <= height, "height {height}");
        }
    }

    #[test]
    fn the_fit_verdict_survives_narrow_rows() {
        let theme = Theme::dark();
        let mut long = row("a-very-long-model-name-that-would-crowd-out-the-verdict");
        long.memory.fit = Fit::WontFit;
        let group = selection_pane(vec![long]);
        for width in [48, 60, 80] {
            let lines = group.body(width, 20, &theme);
            let shown = text_of(&lines);
            let line = shown
                .lines()
                .find(|line| line.contains("[MLX]"))
                .unwrap_or_else(|| panic!("installed row missing at width {width}: {shown}"));
            assert!(line.contains("will not fit"), "{width}: {line}");
            assert!(text::width(line) <= width, "{width}: {line}");
        }
    }

    #[test]
    fn find_view_shows_results_with_counts_and_a_list_files_footer() {
        let theme = Theme::dark();
        let keys = Keymap::defaults();
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.focus = Focus::Results;
        group.curated = false;
        group.hits = vec![Hit {
            repo: "mlx-community/Qwen3-4B-4bit".into(),
            family: Family::Mlx,
            downloads: Some(12_345),
            likes: Some(7),
            created_at: Some("2025-01-01T00:00:00.000Z".into()),
            size_bytes: Some(3 << 30),
            parameters: None,
        }];
        let shown = text_of(&group.body(100, 20, &theme));
        assert!(shown.contains("Qwen3-4B-4bit"), "{shown}");
        assert!(shown.contains("4B"), "{shown}");
        assert!(shown.contains("12.3k"), "{shown}");
        assert!(shown.contains("3.0 GiB"), "{shown}");
        assert!(shown.contains("published"), "{shown}");
        assert!(
            group
                .footer(&keys)
                .iter()
                .any(|hint| hint.contains("list files"))
        );
    }

    #[test]
    fn quantization_picker_lists_each_choice_with_its_size() {
        let theme = Theme::dark();
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.picker = Some(Picker {
            repo: "org/gguf".into(),
            quants: vec![
                Quant {
                    label: "m-Q4_K_M".into(),
                    files: vec!["m-Q4_K_M.gguf".into()],
                    bytes: 2 << 30,
                },
                Quant {
                    label: "m-Q8_0".into(),
                    files: vec!["m-Q8_0.gguf".into(), "mmproj.gguf".into()],
                    bytes: 512 << 20,
                },
            ],
            cursor: 0,
        });
        let shown = text_of(&group.body(90, 20, &theme));
        assert!(shown.contains("m-Q4_K_M"), "{shown}");
        assert!(shown.contains("2.0 GiB"), "{shown}");
        assert!(shown.contains("512 MiB"), "{shown}");
        assert!(shown.contains("2 files"), "{shown}");
    }

    #[test]
    fn window_and_size_helpers_keep_the_cursor_visible_and_read_plainly() {
        assert_eq!(window_start(0, 3), 0);
        assert_eq!(window_start(2, 3), 0);
        assert_eq!(window_start(5, 3), 3);
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(8 << 10), "8 KiB");
        assert_eq!(human_bytes(312 << 20), "312 MiB");
        assert_eq!(human_bytes(3 << 30), "3.0 GiB");
        assert_eq!(compact(None), "n/a");
        assert_eq!(compact(Some(812)), "812");
        assert_eq!(compact(Some(34_500)), "34.5k");
        assert_eq!(compact(Some(1_200_000)), "1.2M");
    }

    #[test]
    fn shelves_cycle_through_mlx_gguf_and_embedding_both_ways() {
        assert_eq!(Shelf::Hub(Family::Mlx).next(), Shelf::Hub(Family::Gguf));
        assert_eq!(Shelf::Hub(Family::Gguf).next(), Shelf::Embedding);
        assert_eq!(Shelf::Embedding.next(), Shelf::Hub(Family::Mlx));
        assert_eq!(Shelf::Hub(Family::Mlx).prev(), Shelf::Embedding);
        assert_eq!(Shelf::Embedding.prev(), Shelf::Hub(Family::Gguf));
    }

    #[test]
    fn arrows_cycle_shelves_and_preserve_navigation_focus() {
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        let keys = Keymap::defaults();
        for shelf in [
            Shelf::Hub(Family::Gguf),
            Shelf::Embedding,
            Shelf::Hub(Family::Mlx),
        ] {
            assert_eq!(group.key(&key(KeyCode::Right), &keys), Step::Handled);
            assert_eq!(group.shelf, shelf);
            assert_eq!(group.focus, Focus::Results);
        }
        group.key(&key(KeyCode::Left), &keys);
        assert_eq!(group.shelf, Shelf::Embedding);
        group.key(&key(KeyCode::BackTab), &keys);
        assert_eq!(group.shelf, Shelf::Hub(Family::Gguf));
    }

    #[test]
    fn suggestions_are_visible_before_many_installed_models_and_exclude_duplicates() {
        let recommendations = download::recommended(Family::Mlx);
        let mut installed: Vec<_> = (0..20).map(|i| row(&format!("installed-{i}"))).collect();
        installed.push(row(&recommendations[0].repo));
        let group = ModelDownloads::fixed(dashboard(installed));
        assert_eq!(group.available_hits(), vec![1, 2]);
        assert!(matches!(group.current(), Some(Pick::Hit(1))));
        let shown = text_of(&group.body(100, 8, &Theme::dark()));
        assert!(shown.contains(&recommendations[1].repo), "{shown}");
        assert!(!shown.contains("installed-0"), "{shown}");
        assert!(!shown.contains(&recommendations[0].repo), "{shown}");
    }

    #[test]
    fn browse_failure_preserves_offline_choices_and_surfaces_error() {
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        let (tx, rx) = mpsc::channel();
        group.search = Some(rx);
        group.browse_attempted = true;
        tx.send(Err("network offline".into())).unwrap();
        group.poll();
        assert_eq!(group.hits, download::recommended(Family::Mlx));
        assert!(group.status.contains("network offline"));
        assert!(
            group.browse_attempted,
            "failed requests must not retry on every draw"
        );
        assert!(group.task.is_none());
        group.reload();
        assert!(!group.browse_attempted, "explicit refresh permits a retry");
    }

    #[test]
    fn background_browse_keeps_selected_model_identity() {
        let mut group = ModelDownloads::fixed(dashboard(vec![row("installed")]));
        let chosen = group.hits[0].repo.clone();
        let (tx, rx) = mpsc::channel();
        group.search = Some(rx);
        let mut hit = group.hits[1].clone();
        hit.repo = "org/new-model".into();
        tx.send(Ok(vec![hit])).unwrap();
        group.poll();
        let Some(Pick::Hit(index)) = group.current() else {
            panic!("selected hit lost")
        };
        assert_eq!(group.hits[index].repo, chosen);
        assert!(group.hits.iter().any(|hit| hit.repo == "org/new-model"));
        assert!(group.task.is_none());
    }

    #[test]
    fn metadata_updates_only_its_model_without_losing_counts_or_selection() {
        let mut group = ModelDownloads::fixed(dashboard(Vec::new()));
        group.hits[0].downloads = Some(123);
        let mut details = group.hits[0].clone();
        details.downloads = None;
        details.size_bytes = Some(2 << 30);
        details.created_at = Some("2025-01-02T00:00:00.000Z".into());
        let (tx, rx) = mpsc::channel();
        group.metadata = Some((details.repo.clone(), rx));
        group.cursor = 1;
        tx.send(Ok(details)).unwrap();
        group.poll();
        assert_eq!(group.cursor, 1);
        assert_eq!(group.hits[0].size_bytes, Some(2 << 30));
        assert_eq!(group.hits[0].downloads, Some(123));
        assert_eq!(group.hits[1].size_bytes, None);
        assert!(group.task.is_none());

        let (tx, rx) = mpsc::channel();
        group.metadata = Some((group.hits[0].repo.clone(), rx));
        tx.send(Ok(group.hits[0].clone())).unwrap();
        group.paste("other");
        assert!(
            group.metadata.is_none(),
            "edits invalidate stale metadata requests"
        );
    }

    #[test]
    fn publication_age_uses_valid_utc_dates_and_never_guesses_missing_values() {
        let now = crate::platform::time::parse_utc("2025-02-01T00:00:00").unwrap();
        assert_eq!(
            published_age(Some("2025-01-31T00:00:00.123Z"), now),
            "1d ago"
        );
        assert_eq!(published_age(Some("2025-02-01T00:00:00Z"), now), "today");
        for date in [
            None,
            Some("invalid"),
            Some("2025-02-30T00:00:00Z"),
            Some("2025-01-01T00:00:00.badZ"),
            Some("2025-01-01T00:00:00+01:00"),
            Some("2026-01-01T00:00:00Z"),
            Some("日本語"),
        ] {
            assert_eq!(published_age(date, now), "unknown", "{date:?}");
        }
        let hit = &download::recommended(Family::Mlx)[0];
        let shown = hit_metadata(hit, now);
        assert!(shown.contains("repo unknown"));
        assert!(shown.contains("published unknown"));
    }
}

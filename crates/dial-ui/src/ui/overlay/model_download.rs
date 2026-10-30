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
use crate::config::paths::Layout;
use crate::llm::catalog::ModelEntry;
use crate::llm::download::{self, Choice, Family, Hit, Quant};
use crate::llm::local_fit::Fit;
use crate::llm::model_config::ModelSelection;
use crate::setup::embeddings;
use crate::stats::local_models::{self, Dashboard, ModelRow, Settings};
use crate::ui::{
    chrome,
    keys::{Action, Keymap},
    text,
    theme::{Theme, Tok},
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    style::Style,
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
    /// Assign the installed model at this path to the memory role (the Memory shelf).
    UseMemory(String),
    /// Assign a discovered image endpoint, preserving its route and never using chat preparation.
    UseImage(ModelSelection),
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

/// The shelves of the group: one Hugging Face weight format, small models for the memory role, a
/// Hugging Face browse shelf for embedding models, or the curated embedding models.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shelf {
    Hub(Family),
    /// Small instruction models for the memory role in this machine's weight format: curated
    /// suggestions (a query searches Hugging Face) and every installed local model. Enter on an
    /// installed model assigns it to the memory role instead of the main one.
    Memory,
    /// MLX text-to-image models, searched by declared Hugging Face pipeline tag.
    Images,
    /// Browse Hugging Face for embedding models (pipeline tag `feature-extraction`); downloads are
    /// GGUF embedders, which Dial then serves through `llama-server` and assigns to the embedding
    /// role. Reuses the Hub download flow but searches embeddings instead of weight formats.
    EmbeddingHub,
    Embedding,
}

const SHELVES: [Shelf; 6] = [
    Shelf::Hub(Family::Mlx),
    Shelf::Hub(Family::Gguf),
    Shelf::Memory,
    Shelf::Images,
    Shelf::EmbeddingHub,
    Shelf::Embedding,
];

impl Shelf {
    fn label(self) -> &'static str {
        match self {
            Shelf::Hub(family) => family.label(),
            Shelf::Memory => "Memory",
            Shelf::Images => "Images",
            Shelf::EmbeddingHub => "Embeddings",
            Shelf::Embedding => "Curated",
        }
    }

    fn next(self) -> Self {
        let at = SHELVES.iter().position(|s| *s == self).unwrap_or(0);
        SHELVES[(at + 1) % SHELVES.len()]
    }

    fn prev(self) -> Self {
        let at = SHELVES.iter().position(|s| *s == self).unwrap_or(0);
        SHELVES[(at + SHELVES.len() - 1) % SHELVES.len()]
    }

    fn family(self) -> Option<Family> {
        match self {
            Shelf::Hub(family) => Some(family),
            Shelf::Memory => Some(download::default_family()),
            Shelf::Images => Some(Family::Mlx),
            Shelf::EmbeddingHub => Some(Family::Gguf),
            Shelf::Embedding => None,
        }
    }

    /// Whether this shelf searches Hugging Face for embedding models rather than weight formats.
    fn embeddable(self) -> bool {
        matches!(self, Shelf::EmbeddingHub)
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
    /// Catalog facts used to keep non-chat checkpoints out of chat and memory shelves.
    known_models: Vec<ModelEntry>,
    /// Image downloads remain classified while the controller's catalog rescan is pending.
    downloaded_images: BTreeSet<String>,
    /// Repositories explicitly selected from Images; survives switching shelves during a download.
    image_repos: BTreeSet<String>,
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
            known_models: Vec::new(),
            downloaded_images: BTreeSet::new(),
            image_repos: BTreeSet::new(),
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
        self.known_models = models.to_vec();
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

    /// Opens the Memory shelf: small local models for the memory role.
    pub fn show_memory(&mut self) {
        if self.shelf != Shelf::Memory {
            self.switch_shelf(Shelf::Memory);
        }
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
        // The Memory shelf keeps its curated small models until the user searches: a popular-models
        // browse would replace them with large general models.
        if self.query.trim().is_empty()
            && !self.browse_attempted
            && self.shelf.family().is_some()
            && self.shelf != Shelf::Memory
        {
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
            // Backspace always edits the query; from the results it returns focus there, unless
            // there is nothing to delete.
            if self.focus == Focus::Query {
                self.query.pop();
                self.query_changed();
            } else if self.query.pop().is_some() {
                self.focus = Focus::Query;
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
                Shelf::Hub(_) | Shelf::Memory | Shelf::EmbeddingHub => "search",
                Shelf::Images => "search images",
                Shelf::Embedding => "list",
            }
        } else {
            match self.current() {
                Some(Pick::Installed(_)) if self.shelf == Shelf::Memory => "use for memory",
                Some(Pick::Installed(_)) if self.shelf == Shelf::Images => "use for images",
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
            "clear search"
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
                Shelf::Images => " Enter searches MLX text-to-image models on Hugging Face; Enter on a result downloads it.".to_owned(),
                Shelf::Memory => {
                    " Enter searches Hugging Face; an empty query shows small models suited to memory extraction."
                        .to_owned()
                }
                Shelf::EmbeddingHub => {
                    " Enter searches Hugging Face for embedding models; an empty query browses popular embedders."
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
        // Any installed chat model can serve the memory role, whatever its weight format.
        let any_format = self.shelf == Shelf::Memory;
        self.dashboard
            .models
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                let image = self.is_image_path(&row.path);
                let compatible = if self.shelf == Shelf::Images {
                    image
                        || self.known_models.iter().any(|entry| {
                            entry.supports_images() && same_model(&entry.selection.model, &row.path)
                        })
                } else {
                    !image
                        && !self.known_models.iter().any(|entry| {
                            same_model(&entry.selection.model, &row.path) && !entry.supports_chat()
                        })
                };
                compatible
                    && (any_format || row.format == Some(family.label()))
                    && self.query_matches(&row.label)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn is_image_path(&self, path: &str) -> bool {
        self.downloaded_images
            .iter()
            .any(|known| same_model(known, path))
            || self.known_models.iter().any(|entry| {
                entry.metadata.kind == crate::llm::catalog::ModelKind::Image
                    && same_model(&entry.selection.model, path)
            })
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
        if self.shelf.embeddable() {
            // Browse popular embedders (or re-run the query) rather than chat-model suggestions.
            self.run_embedding_search();
        } else if self.shelf.family().is_some() && self.query.trim().is_empty() {
            if self.shelf == Shelf::Images {
                self.hits.clear();
                self.curated = false;
                return;
            }
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
            Shelf::Hub(_) | Shelf::Memory | Shelf::EmbeddingHub | Shelf::Images => self
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
                Shelf::Hub(_) | Shelf::Memory | Shelf::Images => {
                    self.run_search();
                    if self.query.trim().is_empty() {
                        self.focus = Focus::Results;
                    }
                }
                Shelf::EmbeddingHub => {
                    self.run_embedding_search();
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
                Some(row) if self.shelf == Shelf::Images || self.is_image_path(&row.path) => {
                    if let Some(entry) = self.known_models.iter().find(|entry| {
                        entry.available
                            && entry.supports_images()
                            && entry.selection.base_url.is_some()
                            && same_model(&entry.selection.model, &row.path)
                    }) {
                        Step::UseImage(entry.selection.clone())
                    } else {
                        self.message = "Image weights are in the Hugging Face cache. Import/serve them in MLX-Serve, then F5 rescan and choose the endpoint in Images.".into();
                        Step::Handled
                    }
                }
                Some(row) if self.shelf == Shelf::Memory => Step::UseMemory(row.path.clone()),
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
            Shelf::Images => {
                self.hits.clear();
                self.curated = false;
                self.search = None;
                self.focus = Focus::Results;
                self.start_search();
            }
            Shelf::Hub(_) | Shelf::Memory if self.query.trim().is_empty() => {
                self.show_curated();
                self.focus = Focus::Results;
            }
            Shelf::Hub(_) | Shelf::Memory => self.run_search(),
            Shelf::EmbeddingHub if self.query.trim().is_empty() => {
                // Browse popular embedders; keep the query so switching back re-searches.
                self.curated = false;
                self.hits.clear();
                self.browse_attempted = true;
                self.metadata = None;
                self.metadata_attempted.clear();
                self.search = None;
                self.cursor = 0;
                self.start_embedding_search();
                self.focus = Focus::Results;
            }
            Shelf::EmbeddingHub => self.run_embedding_search(),
            Shelf::Embedding => {
                self.search = None;
                self.focus = Focus::Results;
                self.refresh_pulled();
            }
        }
    }

    fn show_curated(&mut self) {
        if let Some(family) = self.shelf.family() {
            self.hits = if self.shelf == Shelf::Memory {
                download::recommended_memory(family)
            } else {
                download::recommended(family)
            };
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
        if self.shelf == Shelf::Images {
            self.hits.clear();
            self.curated = false;
            self.cursor = 0;
            self.start_search();
            return;
        }
        if self.query.trim().is_empty() {
            self.show_curated();
            if self.source.is_none() || self.shelf == Shelf::Memory {
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
        if self.shelf == Shelf::Images && self.source.is_none() {
            return;
        }
        let Some(family) = self.shelf.family() else {
            return;
        };
        let query = self.query.trim().to_owned();
        self.browse_attempted = true;
        self.metadata = None;
        self.metadata_attempted.clear();
        let (tx, rx) = mpsc::channel();
        let images = self.shelf == Shelf::Images;
        std::thread::spawn(move || {
            let result = if images {
                download::search_images(&query)
            } else {
                download::search(family, &query)
            };
            let _ = tx.send(result.map_err(|error| format!("{error:#}")));
        });
        self.search = Some(rx);
        self.status = format!("loading {} catalog from Hugging Face…", family.label());
    }

    /// Starts a Hugging Face search for embedding models (pipeline tag `feature-extraction`).
    /// Mirrors [`start_search`] but calls the embedder search and tags results as GGUF, which is
    /// what `llama-server` serves. Network only; no weights are fetched here.
    fn start_embedding_search(&mut self) {
        let query = self.query.trim().to_owned();
        self.browse_attempted = true;
        self.metadata = None;
        self.metadata_attempted.clear();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ =
                tx.send(download::search_embedding(&query).map_err(|error| format!("{error:#}")));
        });
        self.search = Some(rx);
        self.status = "loading embedding models from Hugging Face…".into();
    }

    /// Runs the embedding search for the current query. Empty query browses popular embedders; a
    /// non-empty query clears the list first. Mirrors [`run_search`] but uses the embedder search.
    fn run_embedding_search(&mut self) {
        if self.query.trim().is_empty() {
            if self.source.is_none() {
                return;
            }
        } else {
            self.hits.clear();
            self.curated = false;
            self.cursor = 0;
        }
        self.start_embedding_search();
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
        if self.shelf == Shelf::Images {
            self.image_repos.insert(hit.repo.clone());
        }
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
                if self.query.trim().is_empty()
                    && let Some(hit) = &selected_hit
                    && !hits.iter().any(|new| new.repo == hit.repo)
                {
                    hits.insert(0, hit.clone());
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
        let memory = self.shelf == Shelf::Memory;
        let image = self.image_repos.contains(repo);
        if image {
            self.downloaded_images.insert(path.to_owned());
            self.shelf = Shelf::Images;
        }
        if !memory && !image {
            self.shelf = Shelf::Hub(family);
        }
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
        self.message = if image {
            format!(
                "Downloaded {repo} to the Hugging Face cache. Serve/import it in MLX-Serve, then F5 rescan and choose Images. Compatibility depends on the server's supported packs."
            )
        } else if memory {
            format!("Downloaded {repo}. Enter assigns it to the memory role.")
        } else {
            format!("Downloaded {repo}. Enter makes it the main model.")
        };
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
            let style = chrome::tab_style(theme, shelf == self.shelf);
            spans.push(Span::styled(format!(" {} ", shelf.label()), style));
        }
        Line::from(fit(spans, width))
    }

    fn facts_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        let facts = match self.shelf {
            Shelf::Images => " MLX text-to-image weights · Enter downloads to HF cache · requires a compatible image server, not Dial's chat runner".to_owned(),
            Shelf::Memory => {
                let installed = self.dashboard.models.len();
                let memory = match self.dashboard.memory_bytes {
                    Some(bytes) => format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64),
                    None => "unknown".into(),
                };
                format!(
                    " Memory extraction runs on saved turns: a small fast model is enough · {installed} installed · memory {memory}"
                )
            }
            Shelf::Hub(_) | Shelf::EmbeddingHub => {
                let family = self.shelf.family().unwrap_or(Family::Gguf);
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
                Shelf::Images => "  search MLX text-to-image models on Hugging Face",
                Shelf::Memory => "  type a name, Enter searches Hugging Face for small models",
                Shelf::EmbeddingHub => "  type to search embedders, Enter searches Hugging Face",
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
            Shelf::Hub(_) | Shelf::Memory | Shelf::EmbeddingHub | Shelf::Images => {
                let memory = self.shelf == Shelf::Memory;
                let family = self.shelf.family().unwrap_or(Family::Gguf);
                let available = self.available_hits();
                rows.push(heading(if available.is_empty() {
                    "No new models match · try a search"
                } else if memory && self.curated {
                    "Small models for memory · download, then Enter assigns the memory role"
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
                    rows.push(heading(if memory {
                        "Installed on this machine · Enter assigns the memory role"
                    } else {
                        "Installed on this machine"
                    }));
                } else if memory && self.query.trim().is_empty() {
                    rows.push(heading("No local models installed yet."));
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
    let name = text::one_line(&format!("{}/{}", entry.display_provider(), entry.label));
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
mod tests;

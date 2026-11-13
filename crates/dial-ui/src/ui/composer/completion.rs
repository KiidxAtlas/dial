//! Completion: argument and command candidates, background ranking, ghost text and learned words.
use super::super::word_complete::{WordModel, build_vocabulary, completion_from_project};
use super::paste::quote_mention;
use super::{CommandEntry, CommandKind, Composer, is_slash_command};
use unicode_segmentation::UnicodeSegmentation;

impl Composer {
    /// Valid argument continuations supplied by the controller, in preference order. Unlike history,
    /// these are available before a command has ever been run. Never insert display placeholders.
    pub fn set_argument_completions(&mut self, candidates: Vec<String>) {
        self.candidates_generation += 1;
        let mut seen = std::collections::HashSet::new();
        self.argument_completions = candidates
            .into_iter()
            .filter(|candidate| {
                candidate.starts_with('/')
                    && candidate.contains(' ')
                    && !candidate.chars().any(char::is_control)
                    && seen.insert(candidate.clone())
            })
            .collect();
        self.menu = None;
        self.refresh();
    }

    pub fn set_files(&mut self, paths: Vec<String>) {
        let selection = self.menu_selection();
        self.candidates_generation += 1;
        self.scan_vocabulary(&paths);
        self.files = std::sync::Arc::new(
            paths
                .into_iter()
                .map(|name| CommandEntry {
                    name,
                    description: "project file".into(),
                    kind: CommandKind::File,
                    args_hint: None,
                })
                .collect(),
        );
        self.menu = None;
        self.refresh();
        self.restore_selection(selection);
    }

    pub(super) fn complete_menu(&mut self) -> bool {
        let Some(menu) = self.menu.as_ref().filter(|m| !m.matches.is_empty()) else {
            return false;
        };
        let entry = &self.menu_entries(menu)[menu.matches[menu.selected].index];
        let arguments = menu.arguments.is_some();
        let completed = if let Some(start) = menu.file_start {
            let text = &self.editor.text;
            let cursor = self.editor.cursor;
            let mention = format!("@{}", quote_mention(&entry.name));
            if cursor == text.len() {
                format!("{}{mention} ", &text[..start])
            } else {
                // Mid-draft: replace the word before the cursor and step past the separator that
                // already follows it, so the menu closes as it does at the end of the draft.
                let after = &text[cursor..];
                let gap = after.chars().next().map_or(0, char::len_utf8);
                let completed = format!("{}{mention}{after}", &text[..start]);
                self.editor.set(&completed);
                self.editor.cursor = start + mention.len() + gap;
                self.dismissed = None;
                return true;
            }
        } else {
            // Slash commands are not shell words. Preserve the command and its arguments,
            // including any quoting supplied by the suggestion itself.
            let word = &entry.name;
            if arguments {
                format!("/{word}")
            } else {
                format!("/{word} ")
            }
        };
        self.editor.set(&completed);
        self.dismissed = arguments.then_some(completed);
        true
    }

    /// The word being typed at the end of the draft, completed from learned prompts first and then
    /// from this project's identifiers and file names.
    pub(super) fn word_ghost(&self, draft: &str) -> Option<&str> {
        if !self.word_completion {
            return None;
        }
        let start = draft
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let word = &draft[start..];
        self.words
            .complete(word)
            .or_else(|| completion_from_project(&self.project_words, word))
            .filter(|rest| !rest.is_empty())
    }

    /// Learns the words of a submitted prompt and saves the model. Nothing is learned while word
    /// completion is off, so turning it off also stops the file from growing.
    pub(super) fn learn_words(&mut self, prompt: &str) {
        if !self.word_completion || !self.words.learn_prompt(prompt) {
            return;
        }
        let Some(path) = self.word_store.as_deref() else {
            return;
        };
        match self.words.to_bytes() {
            Ok(bytes) => {
                if let Err(error) = crate::config::settings_store::write_private(path, &bytes) {
                    self.word_notice = Some(format!("Could not save learned words: {error:#}"));
                }
            }
            Err(error) => {
                self.word_notice = Some(format!("Could not save learned words: {error:#}"));
            }
        }
    }

    /// Rebuilds the project vocabulary on a worker thread for `paths`, superseding any scan that
    /// is still running. Does nothing until a project root is set.
    pub(super) fn scan_vocabulary(&mut self, paths: &[String]) {
        // Reading up to 32 MB of source only serves ghost-word completion.
        if !self.word_completion {
            return;
        }
        let Some(root) = self.project_root.clone() else {
            return;
        };
        self.vocab_generation += 1;
        let generation = self.vocab_generation;
        self.vocab_cancel
            .store(generation, std::sync::atomic::Ordering::Relaxed);
        let cancel = self.vocab_cancel.clone();
        let tx = self.vocab_tx.clone();
        let files = paths.to_vec();
        std::thread::spawn(move || {
            let index = build_vocabulary(&root, &files, &cancel, generation);
            let _ = tx.send((generation, index));
        });
    }

    /// Turns inline word completion on or off, from `ui.word_completion`.
    pub fn set_word_completion(&mut self, enabled: bool) {
        self.word_completion = enabled;
    }

    /// Sets the project root whose identifiers and file names feed word completion.
    pub fn set_project_root(&mut self, root: std::path::PathBuf) {
        self.project_root = Some(root);
    }

    /// Loads the learned-words file at `store` and saves to it from now on. A missing file is an
    /// empty model; an unreadable or invalid one leaves an empty model and a notice.
    pub fn load_learned_words(&mut self, store: std::path::PathBuf) {
        match crate::platform::atomic::read_private(&store) {
            Ok(Some(bytes)) => match WordModel::from_bytes(&bytes) {
                Ok(model) => self.words = model,
                Err(error) => {
                    self.word_notice = Some(format!("Learned words were reset: {error:#}"));
                }
            },
            Ok(None) => {}
            Err(error) => {
                self.word_notice = Some(format!("Could not read learned words: {error:#}"));
            }
        }
        self.word_store = Some(store);
    }

    /// Takes the pending word-completion load or save problem, if any.
    pub fn take_word_notice(&mut self) -> Option<String> {
        self.word_notice.take()
    }

    pub(super) fn accept_ghost(&mut self) -> bool {
        let Some(suffix) = self.ghost() else {
            return false;
        };
        let completed = format!("{}{suffix}", self.editor.text);
        self.editor.set(&completed);
        self.dismissed = Some(completed);
        true
    }

    // -----------------------------------------------------------------------------------------
    // Derived state
    // -----------------------------------------------------------------------------------------

    /// Slash commands or the trailing `@file` word at the cursor.
    pub(super) fn menu_query(&self) -> Option<(&str, Option<usize>)> {
        if self.search.is_some() {
            return None;
        }
        let text = self.editor.text.as_str();
        let cursor = self.editor.cursor;
        let at_end = cursor == text.len();
        if (!at_end && !self.mention_edit) || self.dismissed.as_deref() == Some(text) {
            return None;
        }
        // An unedited history entry is being browsed, not typed: Up/Down keep walking history
        // instead of landing in the command or file menu. Editing the entry opens it as usual.
        if self
            .history_index
            .is_some_and(|i| self.history.get(i).is_some_and(|entry| *entry == text))
        {
            return None;
        }
        // A mention completes the word that ends at the cursor, also in the middle of a draft.
        if !text[cursor..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
        {
            return None;
        }
        let before = &text[..cursor];
        let start = before
            .rfind(char::is_whitespace)
            .map_or(0, |i| i + before[i..].chars().next().unwrap().len_utf8());
        if let Some(query) = before[start..].strip_prefix('@') {
            return (!self.files.is_empty()).then_some((query, Some(start)));
        }
        if !at_end {
            return None;
        }
        if self.commands.is_empty() {
            return None;
        }
        let rest = text.strip_prefix('/')?;
        // A pasted path is not a command: no menu, and Enter sends it as typed instead of
        // completing a fuzzy command match.
        if rest.chars().any(char::is_control) || !is_slash_command(text) {
            return None;
        }
        (!rest.chars().any(char::is_whitespace) || self.argument_entries(rest).is_some())
            .then_some((rest, None))
    }

    /// Offer only the next word at the current command depth. This prevents nested flags or skill
    /// names from overwhelming the initial subcommand list.
    pub(super) fn argument_entries(&self, query: &str) -> Option<Vec<CommandEntry>> {
        let (prefix, _) = query.rsplit_once(' ')?;
        let root = query.split(' ').next()?;
        let command = self.commands.iter().find(|entry| entry.name == root)?;
        let stem = format!("/{prefix} ");
        let mut seen = std::collections::HashSet::new();
        let entries: Vec<_> = self
            .argument_completions
            .iter()
            .filter_map(|candidate| {
                let next = candidate.strip_prefix(&stem)?.split(' ').next()?;
                if next.is_empty() || !seen.insert(next.to_owned()) {
                    return None;
                }
                Some(CommandEntry {
                    name: format!("{prefix} {next}"),
                    description: String::new(),
                    kind: command.kind,
                    args_hint: None,
                })
            })
            .collect();
        (!entries.is_empty()).then_some(entries)
    }

    /// Applies finished background rankings. A result is kept only while the menu still shows the
    /// query it was computed for, so stale results never reach the screen. Returns `true` when the
    /// open menu changed and the caller should repaint.
    pub fn poll_autocomplete(&mut self) -> bool {
        let mut changed = self.poll_attachments();
        while let Ok((generation, query, file_start, matches)) = self.autocomplete_rx.try_recv() {
            if generation == self.candidates_generation
                && self.menu_query() == Some((query.as_str(), file_start))
                && let Some(menu) = self.menu.as_mut()
                && menu.query == query
                && menu.file_start == file_start
            {
                menu.matches = matches;
                menu.pending = false;
                menu.selected = menu.selected.min(menu.matches.len().saturating_sub(1));
                changed = true;
            }
        }
        while let Ok((generation, index)) = self.vocab_rx.try_recv() {
            if generation == self.vocab_generation {
                self.project_words = index;
                changed = true;
            }
        }
        changed |= self.refresh_history_search();
        changed
    }

    /// Whether a background ranking is outstanding, so the event loop must wake to collect it.
    pub fn autocomplete_pending(&self) -> bool {
        self.menu.as_ref().is_some_and(|menu| menu.pending)
            || self.history_lookup.pending.is_some()
            || self.history_lookup.running
            || self.attachments.pending()
    }

    /// Marks every outstanding background ranking as stale and returns the new latest request id.
    pub(super) fn supersede_rankings(&self) -> u64 {
        self.rank_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// Dim inline continuation from command metadata, then newest history. Only at the end of an
    /// idle, non-empty draft, and never over half a grapheme. Acceptance never submits the draft.
    pub(super) fn ghost(&self) -> Option<&str> {
        let draft = self.editor.text.as_str();
        if self.busy
            || self.search.is_some()
            || self.history_index.is_some()
            || (self.menu_open()
                && self
                    .menu
                    .as_ref()
                    .is_some_and(|menu| menu.arguments.is_none()))
            || draft.is_empty()
            || self.editor.cursor != draft.len()
            || self.dismissed.as_deref() == Some(draft)
        {
            return None;
        }
        if let Some(menu) = self.menu.as_ref().filter(|_| self.menu_open()) {
            let entry = &self.menu_entries(menu)[menu.matches.get(menu.selected)?.index];
            return entry
                .name
                .strip_prefix(&draft[1..])
                .filter(|suffix| !suffix.is_empty());
        }
        let extends = |entry: &str| {
            entry.starts_with(draft)
                && entry.len() > draft.len()
                && entry
                    .grapheme_indices(true)
                    .any(|(offset, _)| offset == draft.len())
        };
        // Names still use the command menu; argument ghosts start after its first space.
        if draft.starts_with('/')
            && draft.contains(' ')
            && let Some(entry) = self
                .argument_completions
                .iter()
                .find(|entry| extends(entry))
        {
            return Some(&entry[draft.len()..]);
        }
        let history = self
            .history
            .iter()
            .rev()
            .find(|entry| extends(entry))
            .map(|entry| &entry[draft.len()..]);
        history.or_else(|| self.word_ghost(draft))
    }
}

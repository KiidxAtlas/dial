//! A one-shot, local result projection for the existing palette. No search service or journal IO.
use crate::ui::text;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnifiedKind {
    Session,
    Help,
}

impl UnifiedKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Session => "Sessions",
            Self::Help => "Help",
        }
    }
}

/// A controller-owned command projection. Matching can never activate it.
pub struct UnifiedEntry {
    pub kind: UnifiedKind,
    pub label: String,
    pub description: String,
    /// Exact existing slash-command payload, without the leading slash.
    pub command: String,
    /// Original bounded summary for the canonical session matching owner; absent for help.
    pub session: Option<crate::storage::session::SessionInfo>,
}

type LoadResult = Result<Vec<UnifiedEntry>, String>;
type Loader = Box<dyn FnOnce() -> LoadResult + Send>;

pub(super) struct UnifiedSearch {
    pub entries: Vec<UnifiedEntry>,
    loader: Option<Loader>,
    pending: Option<Receiver<LoadResult>>,
    error: Option<String>,
}

impl UnifiedSearch {
    pub fn new(entries: Vec<UnifiedEntry>, loader: Loader) -> Self {
        Self {
            entries,
            loader: Some(loader),
            pending: None,
            error: None,
        }
    }

    /// Starts only after explicit opt-in; polls without blocking and never reruns on edits.
    pub fn poll(&mut self) -> bool {
        if let Some(loader) = self.loader.take() {
            let (tx, rx) = mpsc::channel();
            match std::thread::Builder::new()
                .name("palette-sessions".into())
                .spawn(move || {
                    let _ = tx.send(loader());
                }) {
                Ok(_) => self.pending = Some(rx),
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        let result = match self.pending.as_ref().map(Receiver::try_recv) {
            Some(Ok(result)) => result,
            Some(Err(TryRecvError::Disconnected)) => {
                Err("session listing worker disconnected".into())
            }
            _ => return false,
        };
        self.pending = None;
        match result {
            Ok(entries) => self.entries.extend(entries),
            Err(error) => self.error = Some(text::one_line(&error)),
        }
        true
    }

    pub fn status(&self) -> String {
        if let Some(error) = &self.error {
            format!(
                "Session listing unavailable: {error}; actions, settings and help still searchable"
            )
        } else if self.pending.is_some() || self.loader.is_some() {
            "Actions / Settings / Sessions / Help · loading local session summaries…".into()
        } else {
            "Actions / Settings / Sessions / Help · cached newest 200 local session summaries"
                .into()
        }
    }

    pub fn rows(&self, query: &str) -> Vec<usize> {
        let mut rows: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let score = if let Some(session) = &entry.session {
                    // Negate ranks so the shared sort keeps the lowest (best) session rank first.
                    -(crate::ui::session_picker::search_rank(
                        query,
                        &session.title,
                        &crate::ui::session_picker::search_text(session),
                    )? as i64)
                } else {
                    let name = text::fuzzy(query, &entry.label);
                    let other = text::fuzzy(
                        query,
                        &format!(
                            "{} {} {}",
                            entry.command,
                            entry.description,
                            entry.kind.label()
                        ),
                    );
                    i64::from(name.or(other)?.0)
                };
                Some((index, entry.kind, score))
            })
            .collect();
        rows.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        rows.into_iter().map(|row| row.0).collect()
    }
}

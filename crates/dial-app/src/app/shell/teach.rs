//! "Teach this model": state and turn bookkeeping for learning from committed work.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::git::git_output;
use crate::app::shell::Shell;
use crate::config::types::Outcome;
use crate::ui::keys::Action;

/// "Teach this model" watches this many recent turns for a commit of their changes.
const TEACH_PENDING_TURNS: usize = 8;

pub(in crate::app) const TEACH_OFF: &str = "Turn on Teach this model (/settings → Model → Learning) while using a local model to rate turns and teach it from your work.";

#[derive(Clone, PartialEq, Eq)]
pub(super) struct TeachState {
    pub(in crate::app) model: String,
    /// The setting is on.
    wanted: bool,
    /// The learning store is open, so turns are recorded.
    pub(in crate::app) active: bool,
}

impl TeachState {
    pub(in crate::app) fn of(app: &App) -> Self {
        Self {
            model: app.config.model.clone(),
            wanted: app.config.improve.teach,
            active: app.store.is_some(),
        }
    }
}

/// Where a "Teach this model" turn started: HEAD (when in a repository) and the checkpoint counter.
pub(in crate::app) struct TeachStart {
    pub(in crate::app) base: Option<String>,
    pub(super) checkpoint: Option<u64>,
}

impl Shell {
    /// "Teach this model": settle earlier turns whose changes were committed since, then note HEAD
    /// and the checkpoint counter so the end of this turn can tell what it changed.
    pub(super) fn teach_turn_start(&mut self, app: &App, agent: &Agent<'_>) -> Option<TeachStart> {
        app.store.as_ref()?;
        self.settle_commits(app, agent);
        let root = agent.workspace().root().to_path_buf();
        Some(TeachStart {
            base: crate::learn::signals::head(&|args| git_output(&root, args)),
            checkpoint: agent
                .checkpoints()
                .map(|checkpoints| checkpoints.next_turn()),
        })
    }

    pub(super) fn teach_turn_end(
        &mut self,
        app: &App,
        agent: &Agent<'_>,
        trajectory: i64,
        start: TeachStart,
    ) {
        if let (Some(base), Some(first), Some(checkpoints)) =
            (start.base, start.checkpoint, agent.checkpoints())
        {
            // Every checkpointed turn since the start belongs to this one (workers included).
            let mut changed: Vec<String> = (first..checkpoints.next_turn())
                .flat_map(|turn| checkpoints.changed_files(turn))
                .collect();
            changed.sort();
            changed.dedup();
            let root = agent.workspace().root().to_path_buf();
            let git = |args: &[&str]| git_output(&root, args);
            if let Some(change) =
                crate::learn::signals::capture(&git, &root, trajectory, base, &changed)
            {
                self.teach_pending.push(change);
                let excess = self.teach_pending.len().saturating_sub(TEACH_PENDING_TURNS);
                self.teach_pending.drain(..excess);
                // The turn may have committed its own work.
                self.settle_commits(app, agent);
            }
        }
        if !self.teach_hint_shown {
            self.teach_hint_shown = true;
            self.note(
                "Teaching",
                format!(
                    "{} marks a turn good and {} bad. Committing a turn's changes also counts as good; /undo counts as bad.",
                    self.keys.label(Action::RateGood),
                    self.keys.label(Action::RateBad)
                ),
            );
        }
        self.teach_nudge(app);
    }

    /// Announce what changed for "Teach this model". Each model has its own learning database, so
    /// turn references do not survive a model change.
    pub(super) fn teach_changed(&mut self, app: &App) {
        let now = TeachState::of(app);
        let before = std::mem::replace(&mut self.teach_state, now.clone());
        if before.model != now.model {
            self.last_trajectory = None;
            self.teach_pending.clear();
        }
        if before == now {
            return;
        }
        let message = if !now.wanted {
            if !before.active {
                return;
            }
            "Teaching off: sessions are no longer recorded.".to_owned()
        } else if !app.config.teaching() {
            format!(
                "Teach this model works with local models; {} is hosted, so this session is not recorded.",
                now.model
            )
        } else if let Some(error) = &app.store_error {
            format!("Teaching is on, but the learning store could not open: {error}")
        } else if before.active && before.model == now.model {
            return;
        } else {
            format!(
                "Teaching {}: turns you approve become training examples. /improve train trains and tests a new version; it serves only if it scores better.",
                now.model
            )
        };
        self.note("Teaching", message);
    }

    /// Credit pending turns whose exact changes have since been committed.
    pub(super) fn settle_commits(&mut self, app: &App, agent: &Agent<'_>) {
        let Some(store) = app.store.as_ref() else {
            return;
        };
        if self.teach_pending.is_empty() {
            return;
        }
        let root = agent.workspace().root().to_path_buf();
        let git = |args: &[&str]| git_output(&root, args);
        let (mut kept, mut errors) = (0, Vec::new());
        self.teach_pending.retain(|change| {
            if crate::learn::signals::committed(&git, change) != Some(true) {
                return true;
            }
            let outcome = Outcome {
                trajectory: change.trajectory,
                score: 100.0,
                source: "git:commit".into(),
                reference: None,
            };
            match store.implicit_outcome(&outcome) {
                Ok(true) => kept += 1,
                Ok(false) => {}
                Err(error) => errors.push(format!("{error:#}")),
            }
            false
        });
        match kept {
            0 => {}
            1 => self.note(
                "Teaching",
                "Committed a turn's changes: saved as a good example.",
            ),
            n => self.note(
                "Teaching",
                format!("Committed {n} turns' changes: saved as good examples."),
            ),
        }
        for error in errors {
            self.note("Teaching", format!("could not record the commit: {error}"));
        }
    }

    /// Notes the approved-turn count after a turn, once at three quarters of the training threshold
    /// and again on reaching it, so a session hears about it at most twice.
    fn teach_nudge(&mut self, app: &App) {
        let Some(store) = app.store.as_ref() else {
            return;
        };
        let Ok(approved) = store.verified_live_turns() else {
            return;
        };
        let needed = crate::learn::improve::min_examples(&app.config);
        let stage = if approved >= needed {
            2
        } else if approved * 4 >= needed * 3 {
            1
        } else {
            0
        };
        if stage == 0 || stage <= self.teach_nudge_stage {
            return;
        }
        self.teach_nudge_stage = stage;
        let next = if stage == 2 {
            "enough to train: /improve session starts training on this session's approved turns"
        } else {
            "/improve session approves this session's turns"
        };
        self.note("Teaching", format!("{approved}/{needed} approved · {next}"));
    }
}

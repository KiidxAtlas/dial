//! Command handlers that need no agent, shared by the idle dispatcher (`command.rs`) and the
//! busy-turn router (`busy.rs`) so both run the same code.

use crate::agent::Agent;
use crate::app::core::App;
use crate::app::drive::Step;
use crate::app::overlay::start_streaming_job;
use crate::app::shell::Shell;
use crate::app::shell::teach::TEACH_OFF;
use crate::config::settings_store::{Change, Scope};
use crate::config::types::Outcome;
use crate::storage::skills;
use anyhow::Result;

impl Shell {
    pub(super) fn accounts_note(&mut self) {
        self.note(
            "Accounts",
            "Usage: /accounts pin (chooses which ChatGPT account this session uses)",
        );
    }

    /// `/profile [name|off]`; `agent` is `None` during a turn (see `Shell::save`).
    pub(super) fn profile_command(
        &mut self,
        app: &App,
        agent: Option<&mut Agent<'_>>,
        args: &str,
    ) -> Result<Step> {
        let config = self.settings_config(app).clone();
        if args.is_empty() {
            self.note(
                "Profiles",
                config
                    .model_profiles
                    .iter()
                    .map(|(name, profile)| {
                        format!("{name}: {} / {}", profile.provider, profile.model)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            return Ok(Step::Done);
        }
        if args != "off" && !config.model_profiles.contains_key(args) {
            self.note(
                "Profile",
                format!(
                    "Unknown profile `{args}`. Define it under [model_profiles] or run /profile to list them."
                ),
            );
            return Ok(Step::Done);
        }
        let changes = if args == "off" {
            vec![Change::Remove {
                path: "model_profile".into(),
            }]
        } else {
            vec![Change::Set {
                path: "model_profile".into(),
                value: args.into(),
            }]
        };
        match self.save(app, agent, &changes, Scope::Project) {
            Ok(step) => Ok(step),
            Err(error) => {
                self.note("Profile", format!("not changed: {error:#}"));
                Ok(Step::Done)
            }
        }
    }

    /// `/laya on|off`; `agent` is `None` during a turn (see `Shell::save`).
    pub(super) fn laya_toggle(
        &mut self,
        app: &App,
        agent: Option<&mut Agent<'_>>,
        enabled: bool,
    ) -> Result<Step> {
        let step = self.save(
            app,
            agent,
            &[Change::Set {
                path: "laya.enabled".into(),
                value: toml::Value::Boolean(enabled),
            }],
            Scope::Project,
        )?;
        self.note(
            "Laya",
            if enabled {
                "Decision advice enabled for this project. /laya check probes Dial's managed service."
            } else {
                "Decision advice disabled for this project. The managed service can shut down when idle."
            },
        );
        Ok(step)
    }

    /// `/skills`: install and update run as a background job; the rest is local file work.
    pub(super) fn skills_command(&mut self, app: &App, args: &str) {
        let words = crate::runtime::arguments::split_words(args);
        if matches!(
            words.first().map(String::as_str),
            Some("install" | "update")
        ) {
            // Network-bound: run in the job slot so the UI keeps responding; progress streams in.
            let layout = app.layout.clone();
            let registries = app.config.skills.registries.clone();
            let started =
                start_streaming_job(&mut self.auth_job, "Skills", move |cancel, progress| {
                    skills::management::command(&layout, &registries, words, &cancel, progress)
                });
            if !started {
                self.note(
                    "Skills",
                    "Another background job is running; try again when it finishes.",
                );
            }
        } else {
            let result = skills::management::command(
                &app.layout,
                &app.config.skills.registries,
                words,
                &std::sync::atomic::AtomicBool::new(false),
                &mut |_: &str| {},
            );
            match result {
                Ok(text) => self.note("Skills", text),
                Err(error) => self.note("Skills", format!("{error:#}")),
            }
        }
        self.refresh_commands(app);
    }

    /// `/code-history`. `clear` asks for confirmation first, so the busy router leaves it for idle.
    pub(super) fn code_history(&mut self, app: &App, args: &str) {
        use crate::tools::knowledge::history::{self, ChangeHistory, Request};
        if !(app.config.codebase.enabled && app.config.codebase.history) {
            self.note(
                "Code history",
                "Code history is off. Turn it on in /settings → Project understanding → Code change history (codebase.history).",
            );
            return;
        }
        let request = match history::parse_request(args) {
            Ok(request) => request,
            Err(usage) => {
                self.note("Code history", usage);
                return;
            }
        };
        if request == Request::Clear
            && self.ask(
                "Clear code history",
                "Delete every recorded diff for this project? Files are not changed.",
                &[("Clear code history", 'y'), ("Keep it", 'n')],
            ) != Some(0)
        {
            return;
        }
        let text = ChangeHistory::open(&app.layout).and_then(|store| match request {
            Request::Search {
                query,
                path,
                branch,
            } => {
                let found = store.search(&query, path.as_deref(), branch.as_deref(), 30)?;
                Ok(format!(
                    "{}\n/code-history show <id> prints a diff · filter with path:<file|dir> branch:<name>",
                    history::render_list(&found)
                ))
            }
            Request::Show(id) => Ok(match store.show(id)? {
                Some((version, diff)) => history::render_version(&version, &diff),
                None => format!("No recorded version #{id}."),
            }),
            Request::Clear => Ok(format!("Cleared {} recorded versions.", store.clear()?)),
        });
        self.note(
            "Code history",
            text.unwrap_or_else(|error| format!("{error:#}")),
        );
    }

    pub(super) fn understanding_clear(&mut self, app: &App) {
        let result = crate::tools::knowledge::codebase::Codebase::open(&app.layout)
            .and_then(|cache| cache.clear_architecture());
        match result {
            Ok(removed) => self.note(
                "Understanding",
                format!("Cleared saved architecture synthesis ({removed} records)."),
            ),
            Err(error) => self.note("Understanding", format!("error: {error:#}")),
        }
    }

    /// `/outcome <0-100> [note]`: scores the last completed turn (during a turn, the previous one).
    pub(super) fn outcome_command(&mut self, app: &App, args: &str) {
        let (score, comment) = args
            .split_once(char::is_whitespace)
            .map_or((args, ""), |(score, comment)| (score, comment.trim()));
        match (
            score.parse::<f64>().ok(),
            self.last_trajectory,
            app.store.as_ref(),
        ) {
            (Some(score), Some(trajectory), Some(store)) => {
                let outcome = Outcome {
                    trajectory,
                    score,
                    source: "user".into(),
                    reference: (!comment.is_empty()).then(|| comment.to_owned()),
                };
                match store.outcome(&outcome) {
                    Ok(()) => {
                        self.note(
                            "Outcome",
                            format!("recorded {score} for trajectory {trajectory}"),
                        );
                        self.start_live_monitor("Outcome", app.config.clone());
                    }
                    Err(error) => self.transcript.error(&format!("{error:#}")),
                }
            }
            (None, _, _) => self.note("Outcome", Self::OUTCOME_USAGE),
            (_, None, _) => self.note(
                "Outcome",
                "No completed turn to score yet. Finish a turn, then rate it with /outcome <0-100>.",
            ),
            (_, _, None) => self.note("Outcome", TEACH_OFF),
        }
    }
}

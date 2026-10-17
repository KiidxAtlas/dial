//! The `improve` surface: background watches and the actions behind its commands.

use crate::agent::{self, Agent};
use crate::app::core::App;
use crate::app::display::version_line;
use crate::app::drive::Step;
use crate::app::improve_view::{improve_jobs_text, improve_stop_text, improve_summary};
use crate::app::overlay::{ImproveWatch, Which};
use crate::app::shell::Shell;
use crate::learn::auto;
use crate::storage::store::Store;
use crate::ui::status::Lora;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::time::{Duration, Instant};

impl Shell {
    /// Polls the job started by `/improve cycle` or `train`; notes its outcome once. The scheduler
    /// state is read at most every two seconds, and a queued or running job is not finished yet.
    pub(super) fn poll_improve_watch(&mut self) {
        let Some(watch) = self.improve_watch.as_mut() else {
            return;
        };
        if watch.checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        watch.checked = Instant::now();
        let Ok(store) = Store::open_read_only(&watch.state_dir) else {
            return;
        };
        let status = store
            .get_meta("auto_status")
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or(Value::Null);
        let state = status["state"].as_str().unwrap_or("");
        if matches!(state, "" | "queued" | "running")
            || status["at"].as_u64().unwrap_or(0) < watch.started
        {
            return;
        }
        let error = agent::truncate(status["error"].as_str().unwrap_or("no details"), 300);
        let live = status["active"].as_i64().map_or_else(
            || "the live version is unchanged".to_owned(),
            |id| format!("v{id} is live"),
        );
        let message = match state {
            "idle" => format!("Improvement cycle finished; {live}. /improve status shows the cycle report."),
            "paused" => format!("Improvement paused: {error}. It resumes when the budget allows; /improve enable resumes it now."),
            "stopped" => "Improvement stopped at your request.".to_owned(),
            "disabled" => "Improvement did not run: automatic improvement is paused. /improve enable resumes it.".to_owned(),
            "failed" => format!("Improvement failed: {error}. /improve status shows the last error."),
            other => format!("Improvement ended ({other})."),
        };
        self.improve_watch = None;
        self.note("Improvement", message);
    }

    /// `/improve status` text. Callers settle pending commits first when they have an agent.
    pub(super) fn improve_status_text(&mut self, app: &App) -> String {
        self.learning.read(app);
        let mut summary = improve_summary(app);
        if let Some(monitor) = &self.adapter_monitor {
            let runtime = monitor.lock();
            if let Some(path) = &runtime.adapter {
                summary.push_str(&format!(
                    "\nThis session: {}\nAdapter: {}",
                    Lora::Unknown.with_runtime(&runtime).label(),
                    path.display()
                ));
            } else {
                summary.push_str("\nThis session: no adapter use confirmed.");
            }
        }
        summary
    }

    /// `/improve` and its subcommands. Bare `/improve` opens the panel. Status, jobs and stop also run
    /// during a turn (see `live_command`).
    pub(super) fn improve_command(
        &mut self,
        app: &App,
        agent: &mut Agent<'_>,
        args: &str,
    ) -> Result<Step> {
        let (sub, rest) = args
            .split_once(char::is_whitespace)
            .map_or((args, ""), |(sub, rest)| (sub, rest.trim()));
        match sub {
            "" => return self.open(app, agent, Which::Improve),
            "settings" => return self.open(app, agent, Which::LearningSettings),
            "models" => return self.open(app, agent, Which::LearningModels),
            "status" => {
                self.settle_commits(app, agent);
                let text = self.improve_status_text(app);
                self.note("Improvement", text);
            }
            "jobs" => {
                let text = improve_jobs_text(app);
                self.note("Improvement", text);
            }
            "stop" => {
                let text = improve_stop_text(app, rest);
                self.note("Improvement", text);
            }
            "cycle" | "train" | "session" | "enable" | "disable" | "versions" | "tasks"
            | "rollback" => {
                let text = self
                    .improve_action(app, sub, rest)
                    .unwrap_or_else(|error| format!("{error:#}"));
                self.note("Improvement", text);
            }
            other => self.note(
                "Improvement",
                format!(
                    "Unknown /improve action \"{other}\". Use status, jobs, stop <id>, cycle, train, session, enable, disable, versions, tasks, rollback [version], settings or models. Bare /improve opens the panel."
                ),
            ),
        }
        Ok(Step::Done)
    }

    fn start_improve_watch(&mut self, learning: &crate::config::types::Config) {
        self.improve_watch = Some(ImproveWatch {
            state_dir: learning.state_dir.clone(),
            started: crate::learn::improve::now(),
            checked: Instant::now() - Duration::from_secs(2),
        });
    }

    /// The `/improve` actions that start work, change the switch or read the learning store. Returns
    /// the note text. Starting work needs Enabled on; the Enabled switch is never changed here.
    fn improve_action(&mut self, app: &App, sub: &str, rest: &str) -> Result<String> {
        let learning = app.reload()?;
        let store = App::open_store(&learning)?;
        let off = format!(
            "Automatic improvement is off for this model, so nothing starts. Turn on Enabled in {} first.",
            crate::learn::improve::SETTINGS_PATH
        );
        match sub {
            "session" => {
                let approval = store.approve_session(&self.session_id)?;
                let mut text = format!(
                    "This session: {} turn(s) approved for training, {} already verified, {} kept with an existing lower score, {} not usable (failed or no saved request).",
                    approval.approved, approval.already, approval.judged, approval.unusable
                );
                let verified = store.verified_live_turns()?;
                let needed = crate::learn::improve::min_examples(&learning);
                if verified < needed {
                    text.push_str(&format!(
                        "\n{verified}/{needed} verified turns recorded for this model; training starts once there are {needed}. Run /improve session again after more turns."
                    ));
                    return Ok(text);
                }
                if !learning.improve.enabled {
                    text.push_str(&format!("\nTraining not started. {off}"));
                    return Ok(text);
                }
                ensure!(
                    !crate::learn::compute::paused(&app.layout.user_dir),
                    "{text}\nResume background work first"
                );
                let spawn = auto::start_training(&app.layout, &learning, &store)
                    .map_err(|error| anyhow::anyhow!("{text}\nTraining not started: {error:#}"))?;
                match spawn {
                    auto::Spawn::Started => {
                        self.yield_compute = app.config.uses_local_compute();
                        self.start_improve_watch(&learning);
                        text.push_str("\nTraining and evaluation queued in the background; coding keeps priority. Follow it with /improve jobs; cancel with /improve stop.");
                    }
                    auto::Spawn::Refused(reason) => text.push_str(&format!(
                        "\nTraining not started: {}",
                        reason.message(&learning.state_dir)
                    )),
                }
                Ok(text)
            }
            "cycle" | "train" => {
                ensure!(learning.improve.enabled, "{}", off);
                ensure!(
                    !crate::learn::compute::paused(&app.layout.user_dir),
                    "Resume background work first"
                );
                let spawn = if sub == "train" {
                    auto::start_training(&app.layout, &learning, &store)?
                } else {
                    auto::start_cycle(&app.layout, &learning, &store)?
                };
                spawn.ensure_started(&learning.state_dir)?;
                self.yield_compute = app.config.uses_local_compute();
                self.start_improve_watch(&learning);
                let job = if sub == "train" {
                    "Training and evaluation"
                } else {
                    "Improvement cycle"
                };
                Ok(format!(
                    "{job} queued in the background; coding keeps priority. Follow it with /improve jobs; cancel with /improve stop. You will get a note here when it finishes."
                ))
            }
            "enable" => {
                auto::resume(&learning.state_dir)?;
                store.set_meta("auto_enabled", "true")?;
                auto::maybe_spawn(&app.layout, &learning, &store)?;
                Ok(if learning.improve.enabled {
                    "Automatic improvement resumed for this project.".to_owned()
                } else {
                    format!(
                        "Automatic improvement resumed, but Enabled is still off. Turn it on in {} to run cycles.",
                        crate::learn::improve::SETTINGS_PATH
                    )
                })
            }
            "disable" => {
                store.set_meta("auto_enabled", "false")?;
                Ok("Automatic improvement paused for this project; a running cycle is not interrupted.".to_owned())
            }
            "versions" => {
                let live = store.active().ok().map(|version| version.id);
                let versions = store.versions()?;
                if versions.is_empty() {
                    return Ok("No versions recorded yet.".to_owned());
                }
                Ok(versions
                    .iter()
                    .map(|version| version_line(version, live))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "tasks" => {
                let tasks = crate::learn::repository::load(&learning)?;
                if tasks.is_empty() {
                    return Ok("No repository exercises registered.".to_owned());
                }
                Ok(tasks
                    .iter()
                    .map(|task| {
                        format!(
                            "{} · {} · {} files · {}",
                            task.spec.id,
                            task.spec.split,
                            task.files.len(),
                            task.digest.chars().take(12).collect::<String>()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "rollback" => {
                let confirmed = rest.split_whitespace().any(|word| word == "--yes");
                let wanted = rest
                    .split_whitespace()
                    .find(|word| *word != "--yes")
                    .map(|word| word.trim_start_matches('v').parse::<i64>())
                    .transpose()
                    .context("usage: /improve rollback [version]")?;
                let active = store.active()?;
                let parent = active
                    .parent
                    .context("the live version has no previous version to restore")?;
                ensure!(
                    wanted.is_none_or(|id| id == parent),
                    "only v{parent} can be restored: it is the previous version of the live v{}. /improve versions lists them.",
                    active.id
                );
                if !confirmed
                    && self.ask(
                        "Roll back learning",
                        &format!(
                            "Make v{parent} live again. v{} stays recorded; the change applies from the next turn.",
                            active.id
                        ),
                        &[("Roll back", 'r'), ("Cancel", 'c')],
                    ) != Some(0)
                {
                    return Ok(format!("Rollback cancelled. v{} stays live.", active.id));
                }
                // The same checks as `dial improve rollback`: no cycle may hold the version store.
                let control = auto::lock_file(&learning.state_dir.join("control.lock"))?;
                ensure!(
                    fs2::FileExt::try_lock_exclusive(&control).is_ok(),
                    "an improvement job is running; stop it with /improve stop first"
                );
                crate::learn::improve::verify_adapter(&store, &store.version(parent)?)?;
                let restored = store.rollback()?;
                Ok(format!(
                    "Rolled back: v{} is live again; v{} stays recorded. The change applies from the next turn.",
                    restored.id, active.id
                ))
            }
            other => anyhow::bail!("unknown /improve action {other}"),
        }
    }
}

//! The session side of the `/watch` pane: publishes the live dashboard (`cli::watch::state`) while
//! a pane is attached and runs the requests the pane queues.
//!
//! Every value comes from the owner the main window already uses (the todo mirror, the worker
//! board, the `bg` tool, the slash-command job slot, side threads and the peer board), so the pane
//! never shows a second version of anything. Requests go back through the same owners: `bg` kills,
//! live-task cancellation, the job slot's cancel flag, `/btw cancel` and the `peers` send action.
//! Peer messages need the idle agent, so a message queued during a turn is sent when it ends.
use super::Shell;
use crate::agent::Agent;
use crate::app::core::App;
use crate::cli::watch::{
    self,
    state::{self, Exchange, Job, JobKind, PeerRow, Peers, Request, Snapshot, Thread, Worker},
};
use crate::storage::side_thread::Outcome;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const PUBLISH_EVERY: Duration = Duration::from_millis(500);
/// Unchanged state is still rewritten this often, so the pane can tell a quiet session from a gone one.
const HEARTBEAT: Duration = Duration::from_secs(2);
const ATTACH_EVERY: Duration = Duration::from_secs(1);
const PEERS_EVERY: Duration = Duration::from_secs(3);
const OUTCOMES: usize = 5;
const COMMAND_JOBS: usize = 8;
const THREADS: usize = 20;
const EXCHANGES: usize = 20;
const ACTIVITY: usize = 20;

/// A slash-command job (`/update`, `/gc`, …) as the pane shows it.
struct CommandJob {
    label: String,
    running: bool,
    status: String,
    output: VecDeque<String>,
}

#[derive(Default)]
pub(in crate::app) struct LivePane {
    attached: bool,
    checked_at: Option<Instant>,
    published_at: Option<Instant>,
    last: Option<Snapshot>,
    commands: VecDeque<CommandJob>,
    peers: Peers,
    peers_at: Option<Instant>,
    outcomes: VecDeque<String>,
    /// Peer messages waiting for the idle agent.
    held: Vec<Request>,
}

impl LivePane {
    /// Whether a pane was attached at the last check. Cheap; for paint code.
    pub(in crate::app) fn attached(&self) -> bool {
        self.attached
    }

    fn check(&mut self, layout: &crate::config::paths::Layout, session: &str) -> bool {
        if self
            .checked_at
            .is_none_or(|at| at.elapsed() >= ATTACH_EVERY)
        {
            self.checked_at = Some(Instant::now());
            self.attached = !session.is_empty() && watch::attached(layout, session);
        }
        self.attached
    }

    fn command(&mut self, label: &str) -> &mut CommandJob {
        let found = self
            .commands
            .iter()
            .rposition(|job| job.label == label && job.running);
        let index = match found {
            Some(index) => index,
            None => {
                if self.commands.len() == COMMAND_JOBS {
                    self.commands.pop_front();
                }
                self.commands.push_back(CommandJob {
                    label: label.to_owned(),
                    running: true,
                    status: "running".into(),
                    output: VecDeque::new(),
                });
                self.commands.len() - 1
            }
        };
        &mut self.commands[index]
    }

    fn line(&mut self, label: &str, line: &str) {
        let job = self.command(label);
        for line in line.lines() {
            if job.output.len() == state::JOB_LINES {
                job.output.pop_front();
            }
            job.output
                .push_back(state::bounded(line, state::TEXT_LIMIT));
        }
    }

    fn finish(&mut self, label: &str, status: String) {
        let job = self.command(label);
        job.running = false;
        job.status = status;
    }

    fn outcome(&mut self, text: String) {
        if self.outcomes.len() == OUTCOMES {
            self.outcomes.pop_front();
        }
        self.outcomes.push_back(text);
    }
}

impl Shell {
    /// A progress line from the slash-command job. With a pane attached it goes only to the pane;
    /// the transcript keeps the job's one-line pending notice and its result.
    pub(in crate::app) fn job_line(&mut self, app: &App, label: &str, line: &str) {
        self.live_pane.line(label, line);
        if !self.live_pane.check(&app.layout, &self.session_id) {
            self.transcript.notice_merge(label, line);
        }
    }

    /// Records how the slash-command job ended, for the pane's Jobs tab.
    pub(in crate::app) fn job_ended(&mut self, label: &str, result: &anyhow::Result<String>) {
        let status = match result {
            Ok(_) => "finished".to_owned(),
            Err(error) => format!(
                "failed: {}",
                crate::ui::text::one_line(&format!("{error:#}"))
            ),
        };
        if let Ok(message) = result
            && !message.trim().is_empty()
        {
            self.live_pane.line(label, message);
        }
        self.live_pane.finish(label, status);
    }

    /// Runs queued pane requests and republishes the dashboard when due. `agent` is `None` during a
    /// turn: peer messages then wait and the peer board keeps its last reading.
    pub(in crate::app) fn sync_live_pane(&mut self, app: &App, mut agent: Option<&mut Agent<'_>>) {
        if self.session_id.is_empty() || !watch::enabled(&app.layout, &self.session_id) {
            return;
        }
        let busy = agent.is_none();
        match state::drain(&app.layout, &self.session_id) {
            Ok(requests) => {
                for request in requests {
                    match request {
                        Ok(request) => self.live_pane.held.push(request),
                        Err(error) => self.live_pane.outcome(format!("Refused: {error:#}")),
                    }
                }
            }
            Err(error) => self
                .live_pane
                .outcome(format!("Cannot read pane requests: {error:#}")),
        }
        for request in std::mem::take(&mut self.live_pane.held) {
            if matches!(request, Request::MessagePeer { .. }) && agent.is_none() {
                self.live_pane.held.push(request);
                continue;
            }
            let text = self.run_request(agent.as_deref_mut(), request);
            self.note("Side pane", &text);
            self.live_pane.outcome(text);
        }
        if !self.live_pane.check(&app.layout, &self.session_id) {
            return;
        }
        if let Some(agent) = agent.as_mut()
            && self
                .live_pane
                .peers_at
                .is_none_or(|at| at.elapsed() >= PEERS_EVERY)
        {
            self.live_pane.peers_at = Some(Instant::now());
            self.live_pane.peers = peers(agent);
        }
        let due = self.live_pane.published_at;
        if due.is_some_and(|at| at.elapsed() < PUBLISH_EVERY) {
            return;
        }
        let snapshot = self.live_snapshot(busy);
        if due.is_some_and(|at| at.elapsed() < HEARTBEAT)
            && self.live_pane.last.as_ref() == Some(&snapshot)
        {
            return;
        }
        let mut published = snapshot.clone();
        published.updated_ms = crate::ui::subagent_model::epoch_ms();
        if let Err(error) = state::publish(&app.layout, &self.session_id, &published) {
            // Shown once per failure streak: the next success clears `last`.
            if self.live_pane.last.is_some() || self.live_pane.published_at.is_none() {
                self.note("Side pane", format!("Could not publish: {error:#}"));
            }
            self.live_pane.last = None;
        } else {
            self.live_pane.last = Some(snapshot);
        }
        self.live_pane.published_at = Some(Instant::now());
    }

    fn run_request(&mut self, agent: Option<&mut Agent<'_>>, request: Request) -> String {
        match request {
            Request::StopProcess { name } => match &self.background_control {
                Some(background) => background
                    .kill_job(&name)
                    .map(|done| format!("Stopped from the side pane: {done}"))
                    .unwrap_or_else(|error| format!("Could not stop {name}: {error}")),
                None => "Background job control is unavailable".into(),
            },
            Request::CancelCommand { label } => {
                if self.auth_job.as_ref().is_some_and(|job| job.label == label) {
                    // Dropping the job raises its cancel flag; the main loop settles its notice.
                    self.auth_job.take();
                    self.live_pane.finish(&label, "cancelled".into());
                    format!("Cancelled {label} from the side pane")
                } else {
                    format!("{label} is no longer running")
                }
            }
            Request::CancelWorker { id } => match &self.live_tasks {
                Some(live) => match live.cancel(&id) {
                    Ok(_) => format!("Cancellation requested for worker {id}"),
                    Err(error) => format!("Could not cancel worker {id}: {error:#}"),
                },
                None => "Worker control is unavailable".into(),
            },
            Request::CancelSideAnswer => {
                if self.side.cancel() {
                    "Stopping the streaming side answer".into()
                } else {
                    "No side answer is streaming".into()
                }
            }
            Request::MessagePeer { to, text } => {
                let Some(agent) = agent else {
                    return "Peer messages need the idle session".into();
                };
                let output = agent.peer_coordination(
                    &serde_json::json!({"action": "send", "to": to, "text": text}),
                );
                if output.is_error {
                    format!("Message to {} failed: {}", short(&to), output.text)
                } else {
                    format!("Message queued for peer {}", short(&to))
                }
            }
        }
    }

    fn live_snapshot(&self, busy: bool) -> Snapshot {
        let mut jobs = Vec::new();
        if let Some(view) = self
            .background_control
            .as_ref()
            .and_then(|background| background.jobs_view(None))
        {
            for job in view.jobs {
                let output = self
                    .background_control
                    .as_ref()
                    .and_then(|background| background.jobs_view(Some(&job.name)))
                    .map(|view| tail(&view.screen))
                    .unwrap_or_default();
                jobs.push(Job {
                    kind: JobKind::Process,
                    detail: job.command,
                    status: job.status,
                    running: job.running,
                    name: job.name,
                    output,
                });
            }
        }
        let running_label = self.auth_job.as_ref().map(|job| job.label);
        let mut commands: Vec<Job> = self
            .live_pane
            .commands
            .iter()
            .map(|job| Job {
                kind: JobKind::Command,
                name: job.label.clone(),
                detail: String::new(),
                // A job dropped elsewhere (Esc, sign-out) is no longer running.
                running: job.running && running_label == Some(job.label.as_str()),
                status: if job.running && running_label != Some(job.label.as_str()) {
                    "stopped".into()
                } else {
                    job.status.clone()
                },
                output: job.output.iter().cloned().collect(),
            })
            .collect();
        if let Some(label) = running_label
            && !commands.iter().any(|job| job.running && job.name == label)
        {
            commands.push(Job {
                kind: JobKind::Command,
                name: label.to_owned(),
                status: "running".into(),
                running: true,
                ..Job::default()
            });
        }
        // Running first, newest command jobs before older ones.
        commands.reverse();
        jobs.extend(commands);
        jobs.sort_by_key(|job| !job.running);

        let activity = self.workers.activity();
        let workers = self
            .workers
            .agents()
            .iter()
            .map(|agent| {
                let mut recent: Vec<String> = activity
                    .iter()
                    .filter(|event| event.agent_id == agent.id)
                    .map(|event| {
                        state::bounded(
                            &format!("{}: {}", event.title, event.summary),
                            state::TEXT_LIMIT,
                        )
                    })
                    .collect();
                recent.drain(..recent.len().saturating_sub(ACTIVITY));
                Worker {
                    id: agent.id.clone(),
                    name: agent.name.clone(),
                    agent: agent.agent.clone(),
                    status: agent.status.label().into(),
                    task: state::bounded(&agent.task, state::TEXT_LIMIT),
                    current: agent.current.clone(),
                    model: agent.model.clone(),
                    steps: agent.steps,
                    cost_usd: agent.cost_usd,
                    elapsed_ms: agent.elapsed_ms,
                    cancellable: agent.cancellable,
                    activity: recent,
                }
            })
            .collect();

        let streaming = self.side.streaming();
        let mut threads: Vec<Thread> = self
            .side
            .threads()
            .iter()
            .map(|thread| {
                let skip = thread.exchanges.len().saturating_sub(EXCHANGES);
                Thread {
                    id: thread.id.clone(),
                    title: thread.title.clone(),
                    exchanges: thread
                        .exchanges
                        .iter()
                        .skip(skip)
                        .map(|exchange| Exchange {
                            question: state::bounded(&exchange.question, state::TEXT_LIMIT),
                            answer: state::bounded(&exchange.answer, state::TEXT_LIMIT),
                            outcome: outcome_label(exchange.outcome).into(),
                        })
                        .collect(),
                    streaming: None,
                }
            })
            .collect();
        if let Some((id, question, answer)) = streaming {
            let live = Exchange {
                question: state::bounded(question, state::TEXT_LIMIT),
                answer: state::bounded(answer, state::TEXT_LIMIT),
                outcome: "streaming".into(),
            };
            match threads.iter_mut().find(|thread| thread.id == id) {
                Some(thread) => thread.streaming = Some(live),
                None => threads.push(Thread {
                    id: id.to_owned(),
                    title: crate::ui::text::clip(&crate::ui::text::one_line(question), 60),
                    exchanges: Vec::new(),
                    streaming: Some(live),
                }),
            }
        }
        threads.drain(..threads.len().saturating_sub(THREADS));

        Snapshot {
            version: state::VERSION,
            updated_ms: 0,
            busy,
            plan: self.todo.tasks().to_vec(),
            jobs,
            workers,
            threads,
            peers: self.live_pane.peers.clone(),
            outcomes: self.live_pane.outcomes.iter().cloned().collect(),
        }
    }
}

fn peers(agent: &mut Agent<'_>) -> Peers {
    match agent.peer_board() {
        Ok(board) => Peers {
            error: None,
            scopes: board.scopes,
            overlapping: board.overlapping,
            queued: board.queued,
            rows: board
                .peers
                .into_iter()
                .map(|row| PeerRow {
                    id: row.peer.id,
                    summary: row.peer.summary,
                    phase: row.peer.phase,
                    scopes: row.peer.scopes,
                    overlapping: row.overlapping,
                })
                .collect(),
        },
        Err(error) => Peers {
            error: Some(format!("Project peers are unavailable: {error:#}")),
            ..Peers::default()
        },
    }
}

fn tail(lines: &[String]) -> Vec<String> {
    let mut lines: Vec<&String> = lines.iter().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(state::JOB_LINES);
    lines
        .into_iter()
        .skip(skip)
        .map(|line| state::bounded(line, state::TEXT_LIMIT))
        .collect()
}

fn outcome_label(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Answered => "answered",
        Outcome::Cancelled => "cancelled",
        Outcome::Failed => "failed",
        Outcome::Interrupted => "interrupted",
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests;

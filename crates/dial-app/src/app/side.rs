//! `/btw` side threads: streamed, cancellable side questions kept beside the session.
//!
//! A side request is built from the session snapshot and then runs on its own thread with its own
//! backend, so a main turn can run meanwhile. Results are applied only to the session they were
//! asked in. Switching sessions cancels the running request and drops anything it still sends.

#[cfg(test)]
mod tests;

use crate::agent::Agent;
use crate::agent::modes::btw::SideSnapshot;
use crate::storage::session::Session;
use crate::storage::side_thread::{self, Outcome, SideExchange, SideThread, SideThreads};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, TryRecvError},
};
use std::thread;

const USAGE: &str = "usage: /btw <question> | /btw new <question> | /btw threads | /btw open <id> | /btw copy | /btw promote <id> | /btw cancel";

/// Side threads of the bound session plus at most one streaming request.
#[derive(Default)]
pub(in crate::app) struct SideThreadState {
    session: Option<String>,
    path: Option<PathBuf>,
    threads: SideThreads,
    open: Option<String>,
    job: Option<Job>,
    /// Context of the turn in progress. Busy `/btw` answers from it, never from the running agent.
    snapshot: Option<Arc<SideSnapshot>>,
}

struct Job {
    session: String,
    thread: String,
    question: String,
    cancel: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
    streamed: String,
}

enum Event {
    Text(String),
    Done(Result<String, String>),
}

/// A message for the transcript about a side thread.
pub(in crate::app) struct Note {
    pub title: String,
    pub text: String,
}

impl Drop for SideThreadState {
    fn drop(&mut self) {
        self.cancel_job();
    }
}

impl SideThreadState {
    /// Attaches to a session and its current storage path. Switching or relocating cancels the
    /// running request and recovers the saved prefix without reissuing inference.
    pub(in crate::app) fn bind(&mut self, session: &Session) -> Result<()> {
        let id = session.id();
        let path = session.path().map(side_thread::path_for);
        if self.session.as_deref() == Some(id) && self.path == path {
            return Ok(());
        }
        let mut threads = match &path {
            Some(path) => side_thread::load(path)?,
            None => SideThreads::default(),
        };
        if let Some(pending) = threads.pending.take() {
            let thread = threads
                .thread_mut(&pending.thread)
                .ok_or_else(|| anyhow!("saved side request has no thread {}", pending.thread))?;
            if thread.title.is_empty() {
                thread.title = title_of(&pending.question);
            }
            thread.exchanges.push(SideExchange {
                question: pending.question,
                answer: pending.answer,
                outcome: Outcome::Interrupted,
            });
            if let Some(path) = &path {
                side_thread::save(path, &threads)?;
            }
        }
        self.cancel_job();
        self.threads = threads;
        self.path = path;
        self.session = Some(id.to_string());
        self.open = None;
        self.snapshot = None;
        Ok(())
    }

    pub(in crate::app) fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// The thread being streamed and the text received so far.
    pub(in crate::app) fn streamed(&self) -> Option<(&str, &str)> {
        self.job
            .as_ref()
            .map(|job| (job.thread.as_str(), job.streamed.as_str()))
    }

    /// Threads of the bound session, oldest first.
    pub(in crate::app) fn threads(&self) -> &[SideThread] {
        &self.threads.threads
    }

    /// The streaming request as `(thread, question, answer so far)`.
    pub(in crate::app) fn streaming(&self) -> Option<(&str, &str, &str)> {
        self.job.as_ref().map(|job| {
            (
                job.thread.as_str(),
                job.question.as_str(),
                job.streamed.as_str(),
            )
        })
    }

    /// Freezes the side context for the turn about to run, including its prompt. Busy `/btw`
    /// answers from this snapshot.
    pub(in crate::app) fn prepare_turn(&mut self, agent: &Agent<'_>, prompt: &str) -> Result<()> {
        self.bind(agent.session())?;
        self.snapshot = Some(Arc::new(agent.side_snapshot(Some(prompt))?));
        Ok(())
    }

    /// Drops the turn's snapshot. Running side requests hold their own copy and finish.
    pub(in crate::app) fn end_turn(&mut self) {
        self.snapshot = None;
    }

    /// The snapshot of the turn in progress, if any.
    pub(in crate::app) fn busy_snapshot(&self) -> Option<Arc<SideSnapshot>> {
        self.snapshot.clone()
    }

    /// Whether a `/btw` line asks a question, as opposed to managing threads or promoting.
    pub(in crate::app) fn asks(args: &str) -> bool {
        let verb = args.split_whitespace().next().unwrap_or("");
        !matches!(
            verb,
            "" | "threads" | "open" | "copy" | "cancel" | "promote"
        )
    }

    /// Runs a `/btw` line against `snapshot`. Promotion needs the idle session owner to fork its
    /// saved ancestry; ordinary side commands do not borrow the running main agent.
    pub(in crate::app) fn command(
        &mut self,
        args: &str,
        snapshot: Option<&SideSnapshot>,
    ) -> String {
        match self.command_inner(args, snapshot) {
            Ok(text) => text,
            Err(error) => format!("error: {error:#}"),
        }
    }

    fn command_inner(&mut self, args: &str, snapshot: Option<&SideSnapshot>) -> Result<String> {
        let args = args.trim();
        let (verb, rest) = match args.split_once(char::is_whitespace) {
            Some((verb, rest)) => (verb, rest.trim()),
            None => (args, ""),
        };
        Ok(match verb {
            "" => USAGE.to_string(),
            "threads" => self.list(),
            "cancel" => {
                if self.cancel() {
                    "stopping the streaming side answer".to_string()
                } else {
                    "no side answer is streaming".to_string()
                }
            }
            "open" => self.open(rest)?,
            "copy" => crate::app::clipboard::copy_text(&self.last_answer()?)?.to_string(),
            "promote" => bail!("/btw promote runs between turns"),
            "new" => self.ask(snapshot, rest, true)?,
            _ => self.ask(snapshot, args, false)?,
        })
    }

    fn ask(
        &mut self,
        snapshot: Option<&SideSnapshot>,
        question: &str,
        new: bool,
    ) -> Result<String> {
        let question = question.trim();
        ensure!(!question.is_empty(), "usage: /btw <question>");
        let snapshot = snapshot.ok_or_else(|| anyhow!("no side context for this session yet"))?;
        self.start(snapshot, question, new)
    }

    /// Starts a side question from `snapshot`. `new` opens a fresh thread. Otherwise the open
    /// thread continues, and a new one is created when none is open.
    pub(in crate::app) fn start(
        &mut self,
        snapshot: &SideSnapshot,
        question: &str,
        new: bool,
    ) -> Result<String> {
        let question = question.trim();
        ensure!(!question.is_empty(), "usage: /btw <question>");
        ensure!(
            self.session.as_deref() == Some(snapshot.session_id()),
            "that side context belongs to another session; ask again"
        );
        ensure!(
            !self.busy(),
            "a side answer is still streaming; /btw cancel stops it"
        );
        let thread_id = match self.open.clone() {
            Some(id) if !new && self.threads.thread(&id).is_some() => id,
            _ => {
                let id = self.threads.create(snapshot.anchor().map(str::to_string));
                self.open = Some(id.clone());
                id
            }
        };
        if let Some(session) = self
            .threads
            .thread(&thread_id)
            .and_then(|thread| thread.promoted_session.as_deref())
        {
            bail!(
                "this thread is a full conversation: /resume {session}; /btw new starts another side thread"
            );
        }
        let prior = self
            .threads
            .thread(&thread_id)
            .map(answered_pairs)
            .unwrap_or_default();
        let request = snapshot.request(&prior, question)?;
        let thread = self
            .threads
            .thread_mut(&thread_id)
            .ok_or_else(|| anyhow!("side thread {thread_id} no longer exists"))?;
        let previous = (
            std::mem::replace(&mut thread.anchor, snapshot.anchor().map(str::to_owned)),
            std::mem::replace(
                &mut thread.context_prompt,
                snapshot.pending_prompt().map(str::to_owned),
            ),
            thread.title.is_empty(),
        );
        if previous.2 {
            thread.title = title_of(question);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, events) = mpsc::channel();
        let worker_cancel = cancel.clone();
        let job = Job {
            session: snapshot.session_id().to_string(),
            thread: thread_id.clone(),
            question: question.to_string(),
            cancel,
            events,
            streamed: String::new(),
        };
        let started = self.persist_job(Some(&job)).and_then(|()| {
            thread::Builder::new()
                .name("btw".into())
                .spawn(move || {
                    let mut stream = |text: &str| {
                        let _ = tx.send(Event::Text(text.to_string()));
                    };
                    let result = request
                        .run(worker_cancel, &mut stream)
                        .map_err(|error| format!("{error:#}"));
                    let _ = tx.send(Event::Done(result));
                })
                .map(|_| ())
                .map_err(anyhow::Error::from)
        });
        if let Err(error) = started {
            if let Some(thread) = self.threads.thread_mut(&thread_id) {
                thread.anchor = previous.0;
                thread.context_prompt = previous.1;
                if previous.2 {
                    thread.title.clear();
                }
            }
            return match self.persist_job(None) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "also failed to clear the pending side request: {cleanup:#}"
                ))),
            };
        }
        self.job = Some(job);
        Ok(format!(
            "thread {thread_id} is streaming; /btw cancel stops it"
        ))
    }

    /// Applies streamed text and a finished answer, if any. Results only count for the session
    /// `session_id` that asked for them. `None` means no session is bound, so a running request
    /// is cancelled.
    pub(in crate::app) fn drain(&mut self, session_id: Option<&str>) -> Vec<Note> {
        let Some(mut job) = self.job.take() else {
            return Vec::new();
        };
        if session_id != Some(job.session.as_str()) || self.session.as_deref() != session_id {
            job.cancel.store(true, Ordering::SeqCst);
            return Vec::new();
        }
        let saved_len = job.streamed.len();
        let mut outcome = None;
        loop {
            match job.events.try_recv() {
                Ok(Event::Text(text)) => job.streamed.push_str(&text),
                Ok(Event::Done(result)) => {
                    outcome = Some(result);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    outcome = Some(Err("side request ended unexpectedly".to_string()));
                    break;
                }
            }
        }
        match outcome {
            Some(result) => vec![self.finish(job, result)],
            None => {
                if job.streamed.len() != saved_len
                    && let Err(error) = self.persist_job(Some(&job))
                {
                    // Do not expose a prefix the private store did not accept.
                    job.streamed.truncate(saved_len);
                    job.cancel.store(true, Ordering::SeqCst);
                    let mut note = self.finish(job, Err("side progress could not be saved".into()));
                    note.text = format!("side stream stopped: {error:#}\n{}", note.text);
                    return vec![note];
                }
                self.job = Some(job);
                Vec::new()
            }
        }
    }

    fn finish(&mut self, job: Job, result: Result<String, String>) -> Note {
        let cancelled = job.cancel.load(Ordering::SeqCst);
        let (answer, outcome) = match (result, cancelled) {
            (Ok(answer), true) => (answer, Outcome::Cancelled),
            (Ok(answer), false) => (answer, Outcome::Answered),
            (Err(_), true) => (job.streamed, Outcome::Cancelled),
            (Err(error), false) => {
                let mut answer = job.streamed;
                if !answer.is_empty() {
                    answer.push_str("\n\nerror: ");
                }
                answer.push_str(&error);
                (answer, Outcome::Failed)
            }
        };
        let title = format!("BTW {}", job.thread);
        let Some(thread) = self.threads.thread_mut(&job.thread) else {
            return Note {
                title,
                text: "the side thread no longer exists".to_string(),
            };
        };
        if thread.title.is_empty() {
            thread.title = title_of(&job.question);
        }
        thread.exchanges.push(SideExchange {
            question: job.question,
            answer: answer.clone(),
            outcome,
        });
        let mut text = match outcome {
            Outcome::Answered => answer,
            Outcome::Cancelled => format!("cancelled; partial answer:\n{answer}"),
            Outcome::Interrupted => format!("interrupted; partial answer:\n{answer}"),
            Outcome::Failed => format!("error: {answer}"),
        };
        if let Err(error) = self.persist() {
            text.push_str(&format!("\n(not saved: {error:#})"));
        }
        Note { title, text }
    }

    /// Asks the running request to stop. Its partial answer is kept as a cancelled exchange.
    pub(in crate::app) fn cancel(&mut self) -> bool {
        match &self.job {
            Some(job) => {
                job.cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    fn cancel_job(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Makes `id` the open thread and returns its history for the transcript.
    pub(in crate::app) fn open(&mut self, id: &str) -> Result<String> {
        let thread = self
            .threads
            .thread(id)
            .ok_or_else(|| anyhow!("no side thread {id}"))?;
        let history = render(thread);
        self.open = Some(id.to_string());
        Ok(history)
    }

    /// One line per thread of the bound session.
    pub(in crate::app) fn list(&self) -> String {
        if self.threads.threads.is_empty() {
            return "no side threads in this session; /btw <question> starts one".to_string();
        }
        self.threads
            .threads
            .iter()
            .map(|thread| {
                let open = if self.open.as_deref() == Some(thread.id.as_str()) {
                    " (open)"
                } else {
                    ""
                };
                let promoted = thread
                    .promoted_session
                    .as_ref()
                    .map(|session| format!(" · /resume {session}"))
                    .unwrap_or_default();
                format!(
                    "{}{open} · {} · {} exchange(s){promoted}",
                    thread.id,
                    thread.title,
                    thread.exchanges.len()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The last answered exchange of the open thread, for copying.
    pub(in crate::app) fn last_answer(&self) -> Result<String> {
        let id = self
            .open
            .as_deref()
            .ok_or_else(|| anyhow!("no open side thread; /btw open <id> reopens one"))?;
        let thread = self
            .threads
            .thread(id)
            .ok_or_else(|| anyhow!("no side thread {id}"))?;
        thread
            .exchanges
            .iter()
            .rev()
            .find(|exchange| exchange.outcome == Outcome::Answered)
            .map(|exchange| exchange.answer.clone())
            .ok_or_else(|| anyhow!("thread {id} has no answer yet"))
    }

    /// Promote into an independent full conversation at the saved ancestry. Main history, draft,
    /// running jobs and current branch remain untouched even when main work has advanced.
    pub(in crate::app) fn promote(
        &mut self,
        agent: &Agent<'_>,
        layout: &crate::config::paths::Layout,
        id: &str,
    ) -> Result<String> {
        self.bind(agent.session())?;
        ensure!(
            !self.busy(),
            "wait for the side answer or cancel it before promotion"
        );
        let thread = self
            .threads
            .thread(id)
            .ok_or_else(|| anyhow!("no side thread {id}"))?;
        if let Some(session) = &thread.promoted_session {
            return Ok(session.clone());
        }
        ensure!(
            thread
                .exchanges
                .iter()
                .any(|exchange| exchange.outcome == Outcome::Answered),
            "thread {id} has no completed answers to promote"
        );
        let mut sibling = agent.session().fork_at(layout, thread.anchor.as_deref())?;
        if let Some(prompt) = &thread.context_prompt {
            sibling.append(json!({"role":"user", "content":prompt}))?;
        }
        for exchange in thread
            .exchanges
            .iter()
            .filter(|exchange| exchange.outcome == Outcome::Answered)
        {
            sibling.append(json!({"role":"user", "content":exchange.question}))?;
            sibling.append(json!({"role":"assistant", "content":exchange.answer}))?;
        }
        let session = sibling.id().to_owned();
        if let Some(thread) = self.threads.thread_mut(id) {
            thread.promoted_session = Some(session.clone());
        }
        self.persist().map_err(|error| anyhow!(
            "conversation {session} was created (/resume {session}), but its side-thread link was not saved: {error:#}"
        ))?;
        Ok(session)
    }

    fn persist(&self) -> Result<()> {
        self.persist_job(self.job.as_ref())
    }

    fn persist_job(&self, job: Option<&Job>) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        match job {
            Some(job) => side_thread::save_stream(
                path,
                &self.threads,
                side_thread::PendingRef {
                    thread: &job.thread,
                    question: &job.question,
                    answer: &job.streamed,
                },
            ),
            None => side_thread::save(path, &self.threads),
        }
    }
}

fn answered_pairs(thread: &SideThread) -> Vec<(String, String)> {
    thread
        .exchanges
        .iter()
        .filter(|exchange| exchange.outcome == Outcome::Answered)
        .map(|exchange| (exchange.question.clone(), exchange.answer.clone()))
        .collect()
}

fn render(thread: &SideThread) -> String {
    if thread.exchanges.is_empty() {
        return format!("thread {} is empty", thread.id);
    }
    thread
        .exchanges
        .iter()
        .map(|exchange| {
            let answer = match exchange.outcome {
                Outcome::Answered => exchange.answer.clone(),
                Outcome::Cancelled => format!("(cancelled) {}", exchange.answer),
                Outcome::Interrupted => format!("(interrupted) {}", exchange.answer),
                Outcome::Failed => format!("(failed) {}", exchange.answer),
            };
            format!("Q: {}\nA: {answer}", exchange.question)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn title_of(question: &str) -> String {
    question
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect()
}

//! `/tan <work>`: fork the active session into a background clone that runs one request.
//! The clone is a full `Agent` over the forked session file, driven on a worker thread; the job is
//! tracked in `Jobs` on the parent agent.

use crate::agent::prompts::modes::tan::{CONTEXT_SWITCH, DISPATCH};
use std::collections::BTreeMap;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::agent::safety::approval::PermissionRule;
use crate::agent::workflow::todo::TodoList;
use crate::agent::{Agent, NullUi, make_session_backend};
use crate::config::paths::Layout;
use crate::config::types::{Approval, Config};
use crate::learn::improve::apply_pinned_model;
use crate::storage::session::Session;
use crate::storage::store::Store;

/// Background `/tan` jobs owned by one parent agent. The shell collects finished jobs with
/// [`Agent::finished_tan_jobs`]; anything left over is pruned on the next dispatch.
#[derive(Default)]
pub(crate) struct Jobs {
    next: u64,
    live: BTreeMap<String, Job>,
}

struct Job {
    interrupt: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    /// How the job ended, set by the worker just before `finished`.
    outcome: Arc<Mutex<Option<String>>>,
    _worker: thread::JoinHandle<()>,
}

impl Agent<'_> {
    /// Fork the session into `Tan-<id>` and dispatch `work` to it in the background.
    /// Returns the job id. The clone file lives beside the parent session file.
    pub fn start_tan(&mut self, work: &str) -> Result<String> {
        let work = work.trim();
        if work.is_empty() {
            bail!("Usage: /tan <work>");
        }
        self.config
            .require_model()
            .context("No active model available for /tan.")?;
        let layout = self
            .layout
            .clone()
            .context("/tan requires a persisted session.")?;
        self.session
            .path()
            .context("/tan requires a persisted session.")?;
        self.tan_jobs
            .live
            .retain(|_, job| !job.finished.load(Ordering::SeqCst));
        self.tan_jobs.next += 1;
        let job_id = format!("tan-{}", self.tan_jobs.next);
        let mut clone = self.session.fork(&layout)?;
        let clone_file: PathBuf = clone
            .path()
            .map(PathBuf::from)
            .context("forked session has no file")?;
        if let Err(error) = clone.repair_incomplete_tools() {
            let _ = fs::remove_file(&clone_file);
            return Err(error).context("repairing the forked session");
        }
        let interrupt = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let worker_interrupt = interrupt.clone();
        let worker_finished = finished.clone();
        let worker_outcome = outcome.clone();
        let plan = self.clone_plan();
        let work_owned = work.to_owned();
        let clone_id = clone.id().to_owned();
        let worker_file = clone_file.clone();
        let spawned = thread::Builder::new()
            .name(format!("dial-{job_id}"))
            .spawn(move || {
                let ended = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_clone(
                        plan,
                        layout,
                        clone,
                        &worker_file,
                        worker_interrupt,
                        &work_owned,
                    )
                }));
                let summary = match ended {
                    Ok(Ok(())) => format!("finished; its transcript is session {clone_id}"),
                    Ok(Err(error)) => format!("failed: {error:#}"),
                    Err(_) => "stopped: the background agent panicked".to_owned(),
                };
                *worker_outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(summary);
                worker_finished.store(true, Ordering::SeqCst);
            });
        let worker = match spawned {
            Ok(worker) => worker,
            Err(error) => {
                let _ = fs::remove_file(&clone_file);
                return Err(error).context("starting background tan");
            }
        };
        self.tan_jobs.live.insert(
            job_id.clone(),
            Job {
                interrupt,
                finished,
                outcome,
                _worker: worker,
            },
        );
        let notice = DISPATCH
            .replace("{{jobId}}", &job_id)
            .replace("{{work}}", work);
        self.push(json!({"role":"user","content":notice}))?;
        Ok(job_id)
    }

    /// Abort a `/tan` job and drop it from the registry. The clone's transcript keeps whatever it
    /// recorded before the interrupt. Returns false when the job is unknown or already finished.
    pub fn cancel_tan(&mut self, job_id: &str) -> bool {
        self.tan_jobs.cancel(job_id)
    }

    /// Remove finished jobs and describe how each ended, for the shell to show. Cancelled jobs
    /// are already gone and are not reported again.
    pub fn finished_tan_jobs(&mut self) -> Vec<String> {
        self.tan_jobs.take_finished()
    }

    /// Config, store and approval policy the clone inherits from this agent.
    fn clone_plan(&self) -> ClonePlan {
        let mut config = self.effective_config().clone();
        // The clone reuses the parent's provider cache, keyed by its namespace or session id.
        config.provider_cache_namespace = Some(
            self.auth_config
                .provider_cache_namespace
                .clone()
                .unwrap_or_else(|| self.session.id().to_owned()),
        );
        ClonePlan {
            config,
            store_config: self.store.is_some().then(|| self.config.clone()),
            approval: self.approval(),
            always: self.always.clone(),
        }
    }
}

impl Jobs {
    fn cancel(&mut self, job_id: &str) -> bool {
        let Some(job) = self.live.remove(job_id) else {
            return false;
        };
        let running = !job.finished.load(Ordering::SeqCst);
        job.interrupt.store(true, Ordering::SeqCst);
        running
    }

    fn take_finished(&mut self) -> Vec<String> {
        let done: Vec<String> = self
            .live
            .iter()
            .filter(|(_, job)| job.finished.load(Ordering::SeqCst))
            .map(|(id, _)| id.clone())
            .collect();
        done.into_iter()
            .filter_map(|id| {
                let job = self.live.remove(&id)?;
                let outcome = job.outcome.lock().unwrap_or_else(|e| e.into_inner()).take();
                Some(format!(
                    "Background tan {id} {}",
                    outcome.as_deref().unwrap_or("finished")
                ))
            })
            .collect()
    }

    #[cfg(test)]
    fn insert_idle(&mut self, job_id: &str) {
        self.live.insert(
            job_id.to_owned(),
            Job {
                interrupt: Arc::new(AtomicBool::new(false)),
                finished: Arc::new(AtomicBool::new(false)),
                outcome: Arc::new(Mutex::new(None)),
                _worker: thread::spawn(|| {}),
            },
        );
    }
}

impl Drop for Jobs {
    /// A clone must not outlive its parent agent with an interrupt flag nobody can set.
    fn drop(&mut self) {
        for job in self.live.values() {
            job.interrupt.store(true, Ordering::SeqCst);
        }
    }
}

/// Parent state a clone inherits when it is spawned.
struct ClonePlan {
    config: Config,
    /// Base config used to open the parent's per-model store; `None` when the parent has no store.
    store_config: Option<Config>,
    approval: Approval,
    always: Vec<PermissionRule>,
}

/// Removes the forked session file unless the clone got as far as running its request: a
/// clone that never started has no transcript worth keeping.
struct DiscardUnstarted<'a> {
    file: &'a Path,
    started: bool,
}

impl Drop for DiscardUnstarted<'_> {
    fn drop(&mut self) {
        if !self.started {
            let _ = fs::remove_file(self.file);
        }
    }
}

/// Run the clone on this worker thread: pin the model from the store, inherit approval policy,
/// clear inherited todos, inject the fork notice, run `work`.
fn run_clone(
    plan: ClonePlan,
    layout: Layout,
    session: Session,
    session_file: &Path,
    interrupt: Arc<AtomicBool>,
    work: &str,
) -> Result<()> {
    // Declared first so it drops last, after the clone has released the session file.
    let mut discard = DiscardUnstarted {
        file: session_file,
        started: false,
    };
    let store = plan
        .store_config
        .as_ref()
        .map(|base| Store::open(&base.state_dir, &base.model, &base.use_case, &base.harness))
        .transpose()?;
    let mut config = plan.config;
    if let Some(store) = &store {
        apply_pinned_model(&mut config, store)?;
    }
    let backend = make_session_backend(&config, &session)?;
    let mut clone = Agent::interactive(
        &config,
        backend,
        layout,
        session,
        None,
        store.as_ref(),
        interrupt,
    )?;
    clone.begin_session_lifecycle();
    clone.set_approval(plan.approval);
    clone.always = plan.always;
    clone.todo = TodoList::default();
    // The fork notice makes the parent's objective the parent's alone: the clone never offers or owns it.
    clone.modes.goal = crate::agent::modes::goal::State::default();
    clone.save_modes()?;
    let mut state = clone.session.control_state();
    if !state.is_object() {
        state = json!({});
    }
    state["todo"] = clone.todo.snapshot();
    clone.session.set_control_state(state)?;
    clone.push(json!({"role":"user","content":CONTEXT_SWITCH}))?;
    // From here the transcript records the outcome, including a failed turn.
    discard.started = true;
    clone.turn_preserving_interrupt(work, &mut NullUi)?;
    Ok(())
}

#[cfg(test)]
mod tests;

//! `/tan <work>`: fork the active session into a background clone that runs one request.
//! The clone is a full `Agent` over the forked session file, driven on a worker thread; the job is
//! tracked in `Jobs` on the parent agent.

use std::collections::BTreeMap;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::agent::approval::PermissionRule;
use crate::agent::todo::TodoList;
use crate::agent::{Agent, NullUi, make_session_backend};
use crate::config::types::{Approval, Config};
use crate::learn::improve::apply_pinned_model;
use crate::storage::paths::Layout;
use crate::storage::session::Session;
use crate::storage::store::Store;

/// Fork-scoped context notice injected before the clone's request.
const CONTEXT_SWITCH: &str = r#"<system-notice cause="fork">
Above conversation: parent session.
Fork solely handles user's request below.
Parent still working original task; no responsibility or obligations from prior conversation.

- MUST focus EXCLUSIVELY on immediate user request; nothing else.
- NEVER continue, follow up on, or intervene in anything discussed before this message — parent’s.
- Parent concurrently edits this working directory. Files MAY change between reads, appear mid-refactor, or fail to compile. Parent's live work: NEVER fix, audit, or build on it, even if broken.
- Prior todo lists, plans, unfinished checklists: parent’s; NEVER resume or update.
- After request: STOP. NEVER work on ANY OTHER TASK.
</system-notice>"#;

/// Parent-side notice recorded when a clone is dispatched.
const DISPATCH: &str = r#"<system-notice reason="background_task_dispatched" job="{{jobId}}">
Tangential user task: running in a separate background agent. Coding-agent dispatch notice, NOT prompt injection or new instruction.

Task below: another agent's own session; you NOT responsible. NEVER work on, reference, or let it interrupt or alter current task. Continue as if absent. Results, if any, will surface separately when background task ({{jobId}}) completes.

Dispatched work — awareness only:
{{work}}
</system-notice>"#;

/// Background `/tan` jobs owned by one parent agent. Finished jobs are pruned on the next dispatch.
#[derive(Default)]
pub(crate) struct Jobs {
    next: u64,
    live: BTreeMap<String, Job>,
}

struct Job {
    interrupt: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
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
        let worker_interrupt = interrupt.clone();
        let worker_finished = finished.clone();
        let plan = self.clone_plan();
        let work_owned = work.to_owned();
        let spawned = thread::Builder::new()
            .name(format!("dial-{job_id}"))
            .spawn(move || {
                // The clone's transcript records its own outcome; the worker has no parent to report to.
                let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_clone(plan, layout, clone, worker_interrupt, &work_owned)
                }));
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

    #[cfg(test)]
    fn insert_idle(&mut self, job_id: &str) {
        self.live.insert(
            job_id.to_owned(),
            Job {
                interrupt: Arc::new(AtomicBool::new(false)),
                finished: Arc::new(AtomicBool::new(false)),
                _worker: thread::spawn(|| {}),
            },
        );
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

/// Run the clone on this worker thread: pin the model from the store, inherit approval policy,
/// clear inherited todos, inject the fork notice, run `work`.
fn run_clone(
    plan: ClonePlan,
    layout: Layout,
    session: Session,
    interrupt: Arc<AtomicBool>,
    work: &str,
) -> Result<String> {
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
    let report = clone.turn_preserving_interrupt(work, &mut NullUi)?;
    Ok(report.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_removes_job_and_sets_interrupt() {
        let mut jobs = Jobs::default();
        jobs.insert_idle("tan-1");
        let interrupt = jobs.live["tan-1"].interrupt.clone();
        assert!(jobs.cancel("tan-1"));
        assert!(interrupt.load(Ordering::SeqCst));
        assert!(!jobs.live.contains_key("tan-1"));
        assert!(!jobs.cancel("tan-1"));
    }
}

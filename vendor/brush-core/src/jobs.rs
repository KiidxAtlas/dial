//! Job management

use std::collections::VecDeque;
use std::fmt::Display;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures::FutureExt;
use tokio::sync::watch;

use crate::ExecutionResult;
use crate::error;
use crate::processes;
use crate::sys;
use crate::trace_categories;
use crate::traps;

pub(crate) type JobJoinHandle = tokio::task::JoinHandle<Result<ExecutionResult, error::Error>>;
pub(crate) type JobResult = (Job, Result<ExecutionResult, error::Error>);

/// How long `$!` (and a `kill` of a job that has not started a process yet) waits for a background
/// job to start an external process, or finish, before it proceeds without one. Bounds builtin-only
/// loops (`while :; do :; done &`) that never spawn one.
pub(crate) const BACKGROUND_PID_WAIT: Duration = Duration::from_secs(1);

/// Virtual PIDs are `VIRTUAL_PID_BASE | n` for `n` in `0..2^30`: `0x4000_0000..=0x7FFF_FFFF`.
/// Linux caps real PIDs at 2^22 (`PID_MAX_LIMIT`), macOS at 99999, and Windows process IDs stay far
/// below 2^30 in practice, so no live process carries a virtual PID. Every value fits the `i32`
/// that [`sys::process::ProcessId`] is on every platform.
const VIRTUAL_PID_BASE: u32 = 1 << 30;
const VIRTUAL_PID_MASK: u32 = VIRTUAL_PID_BASE - 1;

/// Counts the virtual PIDs handed out so far in this process.
static VIRTUAL_PIDS: AtomicU32 = AtomicU32::new(1);

/// Returns a new virtual PID, unique among the background jobs of this process until the range
/// wraps after 2^30 jobs.
fn allocate_virtual_pid() -> sys::process::ProcessId {
    let index = VIRTUAL_PIDS.fetch_add(1, Ordering::Relaxed) & VIRTUAL_PID_MASK;
    sys::process::ProcessId::try_from(VIRTUAL_PID_BASE | index).unwrap_or_default()
}

/// The exit status a job ends with when a `kill` signal ends it: `128 + signal`, as a Unix shell
/// reports a signalled child.
pub(crate) fn signal_status(signal: i32) -> ExecutionResult {
    let number = u8::try_from(signal.clamp(0, 127)).unwrap_or_default();
    ExecutionResult::new(128u8.saturating_add(number))
}

/// Publishes the external processes a background job starts, for `$!`, `kill` and the job's
/// `representative_pid`/`process_group_id`.
///
/// The slot is shared (`Arc`) by the job, the `&` launcher and every shell the job clones
/// (subshells, pipeline members), so any external process they start is recorded here. `$!` is the
/// last external member of the first pipeline that starts one (`a | b &` gives `b`, or `a` when `b`
/// is a builtin). A job that starts no external process reports a virtual PID from creation on,
/// so `$!` is never empty after `&` and `kill`/`wait` can name the job. Once the job finishes the
/// slot records nothing further but keeps its PIDs for `wait` and `$!`.
#[derive(Clone)]
pub(crate) struct BackgroundPid(Arc<Slot>);

/// The shared state of one background job's PID slot.
struct Slot {
    /// The PID reported when the job starts no external process.
    virtual_pid: sys::process::ProcessId,
    state: watch::Sender<SlotState>,
}

#[derive(Default)]
struct SlotState {
    /// The PID `$!` reports: the last external member of the first pipeline that started one.
    pid: Option<sys::process::ProcessId>,
    /// Every external process the job has started, in start order.
    externals: Vec<sys::process::ProcessId>,
    /// The job's task has finished; it starts no further processes.
    finished: bool,
    /// The signal a `kill` that ends the job delivered; the job then completes with `128 + signal`.
    killed: Option<i32>,
}

impl BackgroundPid {
    /// Returns a new slot for a job that has started nothing yet.
    pub(crate) fn new() -> Self {
        let (state, _receiver) = watch::channel(SlotState::default());
        Self(Arc::new(Slot {
            virtual_pid: allocate_virtual_pid(),
            state,
        }))
    }

    /// Records the external processes one pipeline started, in pipeline order. Ignored once the
    /// job has finished.
    pub(crate) fn record_started(&self, pids: &[sys::process::ProcessId]) {
        if pids.is_empty() {
            return;
        }
        self.0.state.send_if_modified(|state| {
            if state.finished {
                return false;
            }
            state.externals.extend_from_slice(pids);
            if state.pid.is_none() {
                state.pid = pids.last().copied();
            }
            true
        });
    }

    /// Records that an external process the job started has been reaped. It is no longer
    /// signalled by [`JobManager::signal_job`], since its PID may be reused by an unrelated
    /// process; it still names the job for `$!` and `wait`.
    pub(crate) fn record_exited(&self, pid: sys::process::ProcessId) {
        self.0.state.send_if_modified(|state| {
            let before = state.externals.len();
            state.externals.retain(|recorded| *recorded != pid);
            state.externals.len() != before
        });
    }

    /// Marks the job finished, so the slot stops recording and wakes any waiter.
    pub(crate) fn finish(&self) {
        self.0.state.send_if_modified(|state| {
            let changed = !state.finished;
            state.finished = true;
            changed
        });
    }

    /// Whether the job has finished.
    pub(crate) fn is_finished(&self) -> bool {
        self.0.state.borrow().finished
    }

    /// Every external process the job has started, in start order.
    pub(crate) fn externals(&self) -> Vec<sys::process::ProcessId> {
        self.0.state.borrow().externals.clone()
    }

    /// The recorded external PID `$!` reports, if the job has started one.
    pub(crate) fn external_pid(&self) -> Option<sys::process::ProcessId> {
        self.0.state.borrow().pid
    }

    /// The PID that names the job: the recorded external PID, or else the job's virtual PID.
    pub(crate) fn pid(&self) -> sys::process::ProcessId {
        self.external_pid().unwrap_or(self.0.virtual_pid)
    }

    /// Waits until an external PID is recorded or the job finishes, for at most `limit`, and returns
    /// the job's PID ([`Self::pid`]) at that point.
    pub(crate) async fn wait_for_pid(&self, limit: Duration) -> sys::process::ProcessId {
        let mut receiver = self.0.state.subscribe();
        let settled = async {
            loop {
                let settled = {
                    let state = receiver.borrow_and_update();
                    state.pid.is_some() || state.finished
                };
                if settled {
                    return;
                }
                if receiver.changed().await.is_err() {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout(limit, settled).await;
        self.pid()
    }

    /// Asks the job to end with `signal` (unless it already finished or was already killed).
    pub(crate) fn request_kill(&self, signal: i32) {
        self.0.state.send_if_modified(|state| {
            if state.finished || state.killed.is_some() {
                return false;
            }
            state.killed = Some(signal);
            true
        });
    }

    /// Completes once a `kill` has asked the job to end, yielding the signal number. Never completes
    /// if the slot can no longer be signalled, which cannot happen while the job holds it.
    pub(crate) async fn killed(&self) -> i32 {
        let mut receiver = self.0.state.subscribe();
        loop {
            let killed = receiver.borrow_and_update().killed;
            if let Some(signal) = killed {
                return signal;
            }
            if receiver.changed().await.is_err() {
                return std::future::pending::<i32>().await;
            }
        }
    }

    /// Completes once no external process the job started is still unreaped (see
    /// [`Self::record_exited`]).
    pub(crate) async fn externals_exited(&self) {
        let mut receiver = self.0.state.subscribe();
        loop {
            if receiver.borrow_and_update().externals.is_empty() {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }
}


/// Manages the jobs that are currently managed by the shell.
#[derive(Default)]
pub struct JobManager {
    /// The jobs that are currently managed by the shell.
    pub jobs: Vec<Job>,

    /// The PID slot of the most recent `&` job; backs `$!` even after that job leaves `jobs`.
    last_background: Option<BackgroundPid>,
}

/// Represents a task that is part of a job.
pub enum JobTask {
    /// An external process.
    External(processes::ChildProcess),
    /// An internal asynchronous task.
    Internal(JobJoinHandle),
}

/// Represents the result of waiting on a job task.
pub enum JobTaskWaitResult {
    /// The task has completed.
    Completed(ExecutionResult),
    /// The task was stopped.
    Stopped,
}

impl JobTask {
    /// Returns whether the task is an external process.
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Waits for the task to complete. Returns the result of the wait.
    pub async fn wait(&mut self) -> Result<JobTaskWaitResult, error::Error> {
        match self {
            Self::External(process) => {
                let wait_result = process.wait().await?;
                match wait_result {
                    processes::ProcessWaitResult::Completed(output) => {
                        Ok(JobTaskWaitResult::Completed(output.into()))
                    }
                    processes::ProcessWaitResult::Stopped => Ok(JobTaskWaitResult::Stopped),
                }
            }
            Self::Internal(handle) => Ok(JobTaskWaitResult::Completed(handle.await??)),
        }
    }

    /// Polls the task for completion. Returns `Some(result)` if the task has completed,
    /// or `None` if it is still running. The result is the execution result of the task.
    /// Behaves in a best-effort manner; if an internal error occurs during polling,
    /// it will return `None`.
    fn poll(&mut self) -> Option<Result<ExecutionResult, error::Error>> {
        match self {
            Self::External(process) => {
                let check_result = process.poll();
                check_result.map(|polled_result| polled_result.map(|output| output.into()))
            }
            Self::Internal(handle) => {
                let checkable_handle = handle;
                checkable_handle.now_or_never().and_then(|r| r.ok())
            }
        }
    }
}

impl JobManager {
    /// Returns a new job manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the PID slot of the job just started with `&`, so `$!` can report it.
    pub(crate) fn set_last_background(&mut self, background_pid: BackgroundPid) {
        self.last_background = Some(background_pid);
    }

    /// Returns the PID slot of the most recent `&` job, if any.
    pub(crate) fn last_background(&self) -> Option<BackgroundPid> {
        self.last_background.clone()
    }

    /// Adds a job to the job manager and marks it as the current job;
    /// returns an immutable reference to the job.
    ///
    /// # Arguments
    ///
    /// * `job` - The job to add.
    #[allow(
        clippy::missing_panics_doc,
        reason = "push() guarantees the vector length is >= 1"
    )]
    pub fn add_as_current(&mut self, mut job: Job) -> &Job {
        for j in &mut self.jobs {
            if matches!(j.annotation, JobAnnotation::Current) {
                j.annotation = JobAnnotation::Previous;
                break;
            }
        }

        let id = self.jobs.len() + 1;
        job.id = id;
        job.annotation = JobAnnotation::Current;
        self.jobs.push(job);

        #[allow(clippy::unwrap_used, reason = "we just pushed an element")]
        self.jobs.last().unwrap()
    }

    /// Returns the current job, if there is one.
    pub fn current_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns a mutable reference to the current job, if there is one.
    pub fn current_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns the previous job, if there is one.
    pub fn prev_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// Returns a mutable reference to the previous job, if there is one.
    pub fn prev_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// Tries to resolve the given job specification to a job.
    ///
    /// # Arguments
    ///
    /// * `job_spec` - The job specification to resolve.
    pub fn resolve_job_spec(&mut self, job_spec: &str) -> Option<&mut Job> {
        let remainder = job_spec.strip_prefix('%')?;

        match remainder {
            "%" | "+" => self.current_job_mut(),
            "-" => self.prev_job_mut(),
            s if s.chars().all(char::is_numeric) => {
                let id = s.parse::<usize>().ok()?;
                self.jobs.iter_mut().find(|j| j.id == id)
            }
            _ => {
                tracing::warn!(target: trace_categories::UNIMPLEMENTED, "unimplemented: job spec naming command: '{job_spec}'");
                None
            }
        }
    }

    /// Waits for all managed jobs to complete.
    pub async fn wait_all(&mut self) -> Result<Vec<Job>, error::Error> {
        for job in &mut self.jobs {
            job.wait().await?;
        }

        Ok(self.sweep_completed_jobs())
    }

    /// Returns the index of the job that `target` names: a job spec (`%1`, `%%`, `%+`, `%-`), or
    /// the PID that names the job (`$!`, or the PID `jobs` shows for it).
    fn job_index(&mut self, target: &str) -> Option<usize> {
        if target.starts_with('%') {
            let job_id = self.resolve_job_spec(target)?.id;
            self.jobs.iter().position(|job| job.id == job_id)
        } else {
            let pid: sys::process::ProcessId = target.parse().ok()?;
            self.jobs
                .iter()
                .position(|job| job.representative_pid() == Some(pid))
        }
    }

    /// Returns the PID that names the job `target` (see [`Self::wait_for`]), if a managed job
    /// matches.
    pub fn job_pid(&mut self, target: &str) -> Option<sys::process::ProcessId> {
        let index = self.job_index(target)?;
        self.jobs[index].representative_pid()
    }

    /// Waits for one job, selected by a job spec (`%1`, `%%`, `%+`, `%-`) or by the process ID
    /// `$!` reported for it, removes it once done and returns its exit status. Returns `None`
    /// when no managed job matches, which `wait` reports with exit status 127.
    pub async fn wait_for(&mut self, id: &str) -> Option<Result<ExecutionResult, error::Error>> {
        let index = self.job_index(id)?;
        let result = self.jobs[index].wait().await;
        if matches!(self.jobs[index].state, JobState::Done) {
            self.jobs.remove(index);
        }
        Some(result)
    }

    /// Waits for the next of the jobs `targets` names (every job when `targets` is empty) to
    /// finish, removes it and returns its PID and exit status. Returns `None` when no managed job
    /// matches, which `wait -n` reports with exit status 127.
    pub async fn wait_next(
        &mut self,
        targets: &[String],
    ) -> Option<Result<(Option<sys::process::ProcessId>, ExecutionResult), error::Error>> {
        let indices: Vec<usize> = if targets.is_empty() {
            (0..self.jobs.len()).collect()
        } else {
            let mut found = vec![];
            for target in targets {
                if let Some(index) = self.job_index(target) {
                    if !found.contains(&index) {
                        found.push(index);
                    }
                }
            }
            found
        };
        if indices.is_empty() {
            return None;
        }
        let waits: Vec<_> = self
            .jobs
            .iter_mut()
            .enumerate()
            .filter(|(index, _)| indices.contains(index))
            .map(|(index, job)| Box::pin(async move { (index, job.wait().await) }))
            .collect();
        let ((index, result), _, pending) = futures::future::select_all(waits).await;
        drop(pending);
        let pid = self.jobs[index].representative_pid();
        if matches!(self.jobs[index].state, JobState::Done) {
            self.jobs.remove(index);
        }
        Some(result.map(|status| (pid, status)))
    }

    /// Signals the job that `target` names (see [`Self::job_index`]), returning `None` when no
    /// managed job matches. A background job that has not started an external process yet is given
    /// up to [`BACKGROUND_PID_WAIT`] to start one first. `send` delivers the signal to each external
    /// process the job started and to its foreground external tasks; a process that already exited
    /// (`NotFound`) is not an error. When `terminates` is set the job then ends with status
    /// `128 + signal`, even if it runs no external process.
    pub async fn signal_job(
        &mut self,
        target: &str,
        signal: i32,
        terminates: bool,
        send: &(dyn Fn(sys::process::ProcessId) -> io::Result<()> + Sync),
    ) -> Option<io::Result<()>> {
        let index = self.job_index(target)?;
        let job = &mut self.jobs[index];
        let slot = job.background_pid.clone();
        if let Some(slot) = &slot {
            slot.wait_for_pid(BACKGROUND_PID_WAIT).await;
        }
        let mut pids: Vec<sys::process::ProcessId> = job
            .tasks
            .iter()
            .filter_map(|task| match task {
                JobTask::External(process) => process.pid(),
                JobTask::Internal(_) => None,
            })
            .collect();
        if let Some(slot) = slot.as_ref().filter(|slot| !slot.is_finished()) {
            pids.extend(slot.externals());
        }
        let mut result = Ok(());
        for pid in pids {
            match send(pid) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
        }
        if let (true, Some(slot)) = (terminates, slot.as_ref()) {
            slot.request_kill(signal);
        }
        Some(result)
    }

    /// Polls all managed jobs for completion.
    pub fn poll(&mut self) -> Result<Vec<JobResult>, error::Error> {
        let mut results = vec![];

        let mut i = 0;
        while i != self.jobs.len() {
            if let Some(result) = self.jobs[i].poll_done()? {
                let job = self.jobs.remove(i);
                results.push((job, result));
            } else if matches!(self.jobs[i].state, JobState::Done) {
                // TODO(jobs): This is a workaround to remove jobs that are done but for which we
                // don't know what happened.
                results.push((self.jobs.remove(i), Ok(ExecutionResult::success())));
            } else {
                i += 1;
            }
        }

        Ok(results)
    }

    fn sweep_completed_jobs(&mut self) -> Vec<Job> {
        let mut completed_jobs = vec![];

        let mut i = 0;
        while i != self.jobs.len() {
            if self.jobs[i].tasks.is_empty() {
                completed_jobs.push(self.jobs.remove(i));
            } else {
                i += 1;
            }
        }

        completed_jobs
    }
}

/// Represents the current execution state of a job.
#[derive(Clone)]
pub enum JobState {
    /// Unknown state.
    Unknown,
    /// The job is running.
    Running,
    /// The job is stopped.
    Stopped,
    /// The job has completed.
    Done,
}

impl Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "Unknown"),
            Self::Running => write!(f, "Running"),
            Self::Stopped => write!(f, "Stopped"),
            Self::Done => write!(f, "Done"),
        }
    }
}

/// Represents an annotation for a job.
#[derive(Clone)]
pub enum JobAnnotation {
    /// No annotation.
    None,
    /// The job is the current job.
    Current,
    /// The job is the previous job.
    Previous,
}

impl Display for JobAnnotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, ""),
            Self::Current => write!(f, "+"),
            Self::Previous => write!(f, "-"),
        }
    }
}

/// Encapsulates a set of processes managed by the shell as a single unit.
pub struct Job {
    /// The tasks that make up the job.
    tasks: VecDeque<JobTask>,

    /// If available, the process group ID of the job's processes.
    pgid: Option<sys::process::ProcessId>,

    /// The annotation of the job (e.g., current, previous).
    annotation: JobAnnotation,

    /// The shell-internal ID of the job.
    pub id: usize,

    /// The command line of the job.
    pub command_line: String,

    /// The current operational state of the job.
    pub state: JobState,

    /// For a background job, the slot that reports its first external process's PID.
    background_pid: Option<BackgroundPid>,
}

impl Display for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}]{:3}{}\t{}",
            self.id,
            self.annotation.to_string(),
            self.state,
            self.command_line
        )
    }
}

impl Job {
    /// Returns a new job object.
    ///
    /// # Arguments
    ///
    /// * `children` - The job's known child processes.
    /// * `command_line` - The command line of the job.
    /// * `state` - The current operational state of the job.
    pub(crate) fn new<I>(tasks: I, command_line: String, state: JobState) -> Self
    where
        I: IntoIterator<Item = JobTask>,
    {
        Self {
            id: 0,
            tasks: tasks.into_iter().collect(),
            pgid: None,
            annotation: JobAnnotation::None,
            command_line,
            state,
            background_pid: None,
        }
    }

    /// Attaches the slot that reports this job's first external process's PID.
    pub(crate) fn with_background_pid(mut self, background_pid: BackgroundPid) -> Self {
        self.background_pid = Some(background_pid);
        self
    }

    /// Returns a pid-style string for the job.
    pub fn to_pid_style_string(&self) -> String {
        let display_pid = self
            .representative_pid()
            .map_or_else(|| String::from("<pid unknown>"), |pid| pid.to_string());
        std::format!("[{}]{}\t{}", self.id, self.annotation, display_pid)
    }

    /// Returns the annotation of the job.
    pub fn annotation(&self) -> JobAnnotation {
        self.annotation.clone()
    }

    /// Returns the command name of the job.
    pub fn command_name(&self) -> &str {
        self.command_line
            .split_ascii_whitespace()
            .next()
            .unwrap_or_default()
    }

    /// Returns whether the job is the current job.
    pub const fn is_current(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Current)
    }

    /// Returns whether the job is the previous job.
    pub const fn is_prev(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Previous)
    }

    /// Polls whether the job has completed.
    pub fn poll_done(
        &mut self,
    ) -> Result<Option<Result<ExecutionResult, error::Error>>, error::Error> {
        let mut result: Option<Result<ExecutionResult, error::Error>> = None;

        tracing::debug!(target: trace_categories::JOBS, "Polling job {} for completion...", self.id);

        while !self.tasks.is_empty() {
            let task = &mut self.tasks[0];
            match task.poll() {
                Some(r) => {
                    self.tasks.remove(0);
                    result = Some(r);
                }
                None => {
                    return Ok(None);
                }
            }
        }

        tracing::debug!(target: trace_categories::JOBS, "Job {} has completed.", self.id);

        self.state = JobState::Done;

        Ok(result)
    }

    /// Waits for the job to complete.
    pub async fn wait(&mut self) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        while let Some(task) = self.tasks.back_mut() {
            match task.wait().await? {
                JobTaskWaitResult::Completed(execution_result) => {
                    result = execution_result;
                    self.tasks.pop_back();
                }
                JobTaskWaitResult::Stopped => {
                    self.state = JobState::Stopped;
                    return Ok(ExecutionResult::stopped());
                }
            }
        }

        self.state = JobState::Done;

        Ok(result)
    }

    /// Moves the job to execute in the background.
    pub fn move_to_background(&mut self) -> Result<(), error::Error> {
        if matches!(self.state, JobState::Stopped) {
            if let Some(pgid) = self.process_group_id() {
                sys::signal::continue_process(pgid)?;
                self.state = JobState::Running;
                Ok(())
            } else {
                Err(error::ErrorKind::FailedToSendSignal.into())
            }
        } else {
            error::unimp("move job to background")
        }
    }

    /// Moves the job to execute in the foreground.
    pub fn move_to_foreground(&mut self) -> Result<(), error::Error> {
        if matches!(self.state, JobState::Stopped) {
            if let Some(pgid) = self.process_group_id() {
                sys::signal::continue_process(pgid)?;
                self.state = JobState::Running;
            } else {
                return Err(error::ErrorKind::FailedToSendSignal.into());
            }
        }

        if let Some(pgid) = self.process_group_id() {
            sys::terminal::move_to_foreground(pgid)?;
        }

        Ok(())
    }

    /// Kills the job.
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal to send to the job.
    pub fn kill(&self, signal: traps::TrapSignal) -> Result<(), error::Error> {
        if let Some(pid) = self.process_group_id() {
            sys::signal::kill_process(pid, signal)
        } else {
            Err(error::ErrorKind::FailedToSendSignal.into())
        }
    }

    /// Tries to retrieve a "representative" pid for the job: its first external process, else the
    /// external PID its background slot recorded, else the slot's virtual PID.
    pub fn representative_pid(&self) -> Option<sys::process::ProcessId> {
        self.external_pid()
            .or_else(|| self.background_pid.as_ref().map(BackgroundPid::pid))
    }

    /// The job's first external process's PID, or else the external PID its background slot
    /// recorded. Never a virtual PID.
    fn external_pid(&self) -> Option<sys::process::ProcessId> {
        self.tasks
            .iter()
            .find_map(|task| match task {
                JobTask::External(process) => process.pid(),
                JobTask::Internal(_) => None,
            })
            .or_else(|| self.background_pid.as_ref().and_then(BackgroundPid::external_pid))
    }

    /// Tries to retrieve the process group ID (PGID) of the job. A job that started no external
    /// process has none.
    pub fn process_group_id(&self) -> Option<sys::process::ProcessId> {
        // TODO(jobs): Don't assume that the first PID is the PGID.
        self.pgid.or_else(|| self.external_pid())
    }
}

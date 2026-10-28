//! Cross-session sharing of build and test results through the project run ledger.
//!
//! Before a check command (`cargo test`, `npm run build`, …) runs, the source tree is fingerprinted
//! from git: HEAD plus the content of every uncommitted or untracked file. When another live
//! session in this checkout already finished the exact command on that tree, its result is
//! returned once instead of compiling again; when it is still running, this session joins it and
//! receives the result as a notice. Asking again runs the command here. A shared result is a
//! peer's observation: it never counts as this session's own verification. A published result
//! requires the tree to be unchanged from the run's start to its end.
use crate::agent::*;
use crate::storage::coordination::{RunEntry, RunResult, RunStatus};
use sha2::{Digest as _, Sha256};
use std::process::{Command, Stdio};

const MAX_TRACKED: usize = 64;
const JOIN_POLL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(in crate::agent) struct State {
    /// The announced run of the bash call being dispatched now.
    current: Option<Pending>,
    /// Announced runs that moved to `bg`, keyed by job name.
    jobs: Vec<Pending>,
    /// (command, fingerprint) already answered from a peer once; the next identical call runs here.
    answered: Vec<(String, String)>,
    /// Peer runs this session joined; each is delivered once when it settles.
    joined: Vec<(String, String)>,
    next_poll: Option<Instant>,
}

struct Pending {
    command: String,
    cwd: String,
    fingerprint: String,
    job: Option<String>,
}

fn push_bounded<T>(list: &mut Vec<T>, item: T) {
    if list.len() >= MAX_TRACKED {
        list.remove(0);
    }
    list.push(item);
}

/// Identity of `command` run in `cwd` against the current tree: HEAD and the content of every
/// changed or untracked file. `None` (no sharing) outside git or when any file cannot be hashed.
pub(in crate::agent) fn tree_fingerprint(root: &Path, command: &str, cwd: &str) -> Option<String> {
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            // Never take the index lock: concurrent sessions run git too.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    };
    let head = git(&["rev-parse", "--verify", "--quiet", "HEAD"])?;
    // Porcelain paths are relative to the repository top, which may be above the workspace root.
    let top = git(&["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(
        std::str::from_utf8(&top)
            .ok()?
            .trim_end_matches(['\n', '\r']),
    );
    let status = git(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
    ])?;
    let mut paths = Vec::new();
    for entry in status
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        paths.push(std::str::from_utf8(entry).ok()?.get(3..)?.to_owned());
    }
    let identity = verify::verification_receipts::source_identity(&top, &paths);
    if !identity.complete {
        return None;
    }
    let mut digest = Sha256::new();
    for field in [
        b"dial-shared-run-v1".as_slice(),
        head.trim_ascii(),
        identity.digest.as_bytes(),
        command.as_bytes(),
        cwd.as_bytes(),
    ] {
        digest.update((field.len() as u64).to_le_bytes());
        digest.update(field);
    }
    Some(format!("{:x}", digest.finalize()))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Exit code from a `bg` completion label (`exited (exit code N)`); `None` for signals or errors.
fn completion_exit_code(text: &str) -> Option<i32> {
    text.strip_prefix("exited (exit code ")?
        .split(')')
        .next()?
        .parse()
        .ok()
}

pub(in crate::agent) fn shared_result_text(entry: &RunEntry) -> String {
    let status = match entry.status {
        RunStatus::Passed => "passed",
        RunStatus::Failed => "failed",
        RunStatus::Running => "running",
    };
    let exit = entry
        .exit_code
        .map(|code| format!(" (exit code {code})"))
        .unwrap_or_default();
    let ago = now().saturating_sub(entry.finished_unix.unwrap_or(entry.started_unix));
    let log = entry
        .artifact
        .as_ref()
        .map(|uri| format!(" Complete log: {uri}."))
        .unwrap_or_default();
    format!(
        "[Shared result, not run here: peer session {} ran `{}` on an identical source tree (same HEAD and uncommitted content) {ago}s ago and it {status}{exit}.{log}]\n{}\n[Run the same command again to execute it in this session. A shared result is a peer's observation, not this session's own verification.]",
        entry.session_id, entry.command, entry.summary
    )
}

impl Agent<'_> {
    fn shareable_run(&self, call: &ToolCall) -> Option<(String, String, String)> {
        if self.hermetic || call.name != "bash" {
            return None;
        }
        let command = call.arguments["command"].as_str()?;
        if !crate::agent::memory::selfplay::is_check_command(command) {
            return None;
        }
        self.coordination_participant()?;
        let cwd = call.arguments["cwd"].as_str().unwrap_or_default();
        let fingerprint = tree_fingerprint(self.workspace.root(), command, cwd)?;
        Some((command.to_owned(), cwd.to_owned(), fingerprint))
    }

    /// Answer a check command from a peer's identical run, or announce this session's run.
    pub(in crate::agent) fn shared_run_before(&mut self, call: &ToolCall) -> Option<ToolOutput> {
        self.shared_runs.current = None;
        let (command, cwd, fingerprint) = self.shareable_run(call)?;
        let key = (command.clone(), fingerprint.clone());
        if !self.shared_runs.answered.contains(&key)
            && let Some(entry) = self
                .coordination_participant()?
                .shared_run(&command, &fingerprint)
                .ok()
                .flatten()
        {
            push_bounded(&mut self.shared_runs.answered, key.clone());
            if entry.status == RunStatus::Running {
                push_bounded(&mut self.shared_runs.joined, key);
                return Some(ToolOutput::ok(format!(
                    "[Not started: peer session {} is already running `{command}` on an identical source tree (started {}s ago). You receive its result when it finishes; continue independent work meanwhile. Run the same command again to run it here anyway.]",
                    entry.session_id,
                    now().saturating_sub(entry.started_unix)
                )));
            }
            let mut output = ToolOutput::ok(shared_result_text(&entry));
            output.is_error = entry.status == RunStatus::Failed;
            return Some(output);
        }
        let announced = self.coordination_participant().is_some_and(|peers| {
            peers
                .begin_run(self.session.id(), &command, &fingerprint)
                .is_ok()
        });
        if announced {
            self.shared_runs.current = Some(Pending {
                command,
                cwd,
                fingerprint,
                job: None,
            });
        }
        None
    }

    /// Publish the announced run's result, or keep it pending when it moved to `bg`.
    pub(in crate::agent) fn shared_run_after(&mut self, output: &ToolOutput) {
        let Some(mut run) = self.shared_runs.current.take() else {
            return;
        };
        if let Some(job) = output
            .data
            .as_ref()
            .and_then(|data| data["background_job"].as_str())
        {
            run.job = Some(job.to_owned());
            push_bounded(&mut self.shared_runs.jobs, run);
            return;
        }
        let exit = output
            .process_outcome()
            .filter(|process| process.started && !process.timed_out && !process.cancelled)
            .and_then(|process| process.exit_code);
        let artifact = output
            .execution
            .output
            .as_ref()
            .and_then(|output| output.artifact.clone());
        self.publish_run(&run, exit, &output.text, artifact.as_deref());
    }

    /// A `bg` job finished; publish it when it was an announced run.
    pub(in crate::agent) fn shared_run_job_finished(
        &mut self,
        job: &str,
        label: &str,
        artifact: Option<&str>,
    ) {
        let Some(index) = self
            .shared_runs
            .jobs
            .iter()
            .position(|run| run.job.as_deref() == Some(job))
        else {
            return;
        };
        let run = self.shared_runs.jobs.remove(index);
        self.publish_run(&run, completion_exit_code(label), label, artifact);
    }

    fn publish_run(&self, run: &Pending, exit: Option<i32>, text: &str, artifact: Option<&str>) {
        let Some(peers) = self.coordination_participant() else {
            return;
        };
        let unchanged = exit.is_some()
            && tree_fingerprint(self.workspace.root(), &run.command, &run.cwd).as_deref()
                == Some(run.fingerprint.as_str());
        let result = exit.map(|exit_code| RunResult {
            exit_code,
            summary: text,
            artifact,
            unchanged,
        });
        let _ = peers.finish_run(&run.command, &run.fingerprint, result);
    }

    /// Deliver joined peer runs that settled since the last poll, at most every two seconds.
    pub(in crate::agent) fn poll_joined_runs(&mut self) {
        if self.shared_runs.joined.is_empty()
            || self
                .shared_runs
                .next_poll
                .is_some_and(|at| Instant::now() < at)
        {
            return;
        }
        self.shared_runs.next_poll = Some(Instant::now() + JOIN_POLL);
        if self.coordination_participant().is_none() {
            return;
        }
        let joined = std::mem::take(&mut self.shared_runs.joined);
        let Some(peers) = self.coordination_participant() else {
            return;
        };
        let mut notices = Vec::new();
        let mut still_running = Vec::new();
        for (command, fingerprint) in joined {
            match peers.shared_run(&command, &fingerprint) {
                Ok(Some(entry)) if entry.status == RunStatus::Running => {
                    still_running.push((command, fingerprint));
                }
                Ok(Some(entry)) => notices.push(shared_result_text(&entry)),
                Ok(None) => notices.push(format!(
                    "[The peer run of `{command}` you joined ended without a shareable result (cancelled, timed out, or the tree changed during it). Run it here if you still need it.]"
                )),
                Err(_) => still_running.push((command, fingerprint)),
            }
        }
        self.shared_runs.joined = still_running;
        self.background_notices.extend(notices);
    }
}

#[cfg(test)]
mod tests;

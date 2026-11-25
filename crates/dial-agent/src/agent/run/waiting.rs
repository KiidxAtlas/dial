//! Suspend inference at a yield boundary until owned work has something useful to deliver.
use crate::agent::*;

// Legacy hosts may queue input without signalling the platform wake channel. Keep their input,
// paint, runtime projection and budget checks responsive at the UI heartbeat, but never ask the
// model to poll on this tick.
const HOST_TICK: Duration = crate::agent::ui::chat::HEARTBEAT;
/// How long a settled wait waits for a finished job's trailing completion notification.
const SETTLE_GRACE: Duration = Duration::from_millis(500);
const SETTLE_POLL: Duration = Duration::from_millis(10);
/// An unattended run has nobody to steer a wait, so a running `bash` job (often a service that
/// never exits) is awaited this long at most before the model must decide what to do with it.
pub(in crate::agent) const UNATTENDED_JOB_WAIT: Duration = Duration::from_secs(60);

impl Agent<'_> {
    /// Returns true when a continuation is needed, including user steering, peer mail or a stop
    /// boundary.
    /// Explicit `bg start` services are not awaited; only work already blocking completion counts.
    pub(in crate::agent) fn wait_for_background_event(&mut self, ui: &mut dyn Ui) -> Result<bool> {
        let waiter = crate::platform::wake::Waiter::new()?;
        // Register before observing conditions: completion between the check and sleep leaves a
        // pending wake. A wake is just a hint; all decisions below recheck the owning state.
        self.pump_subagents(ui);
        if self.background_result_ready() {
            return Ok(true);
        }
        // A job the model itself sent to the background (`bash background: true`, often a server
        // that never exits) was not expected to finish within this turn. With no user to steer the
        // wait, report it now instead of spending the whole unattended bound on it.
        if self.modes.launch.unattended {
            let requested: Vec<String> = self
                .requested_background_jobs()
                .into_iter()
                .filter(|job| !self.unattended_wait_noticed.contains(job))
                .collect();
            if !requested.is_empty() {
                self.unattended_wait_noticed
                    .extend(requested.iter().cloned());
                self.background_notices
                    .push(unattended_running_notice(&requested));
                self.record(
                    self.cur_step,
                    "background_wait_skipped",
                    json!({"reason":"requested_background","jobs":requested}),
                )?;
                return Ok(true);
            }
        }
        let jobs = self.waitable_background_jobs();
        let workers = self.live_tasks.running_ids();
        let questions = self.side_questions.running_ids();
        if jobs.is_empty() && workers.is_empty() && questions.is_empty() {
            // Listing the jobs refreshes their table, so one may have finished since the pump above.
            self.pump_subagents(ui);
            return Ok(self.background_result_ready());
        }
        let started = Instant::now();
        let job_bound = (self.modes.launch.unattended && !jobs.is_empty())
            .then(|| started + self.unattended_job_wait);
        self.record(
            self.cur_step,
            "background_wait_started",
            json!({
                "jobs":jobs,"workers":workers,"side_questions":questions
            }),
        )?;
        use crate::agent::ui::ui_event::{BackgroundWait, EventKind, UiEvent};
        ui.event(&UiEvent::new(EventKind::Wait, format!(
            "Waiting for running work without model polling: {}. Results resume the turn; new input changes direction. Interrupting the turn does not cancel these jobs; use Activity to cancel owned work.",
            jobs.iter().chain(&workers).chain(&questions).cloned().collect::<Vec<_>>().join(", ")
        )).with_background_wait(BackgroundWait::Started { jobs, workers, questions }));
        let result = (|| -> Result<&str> {
            Ok(loop {
                // User input wins even when completion arrives in the same wake. It is not consumed
                // here; ordinary step admission routes it before any further inference or effects.
                if self.interrupt.load(Ordering::Acquire) {
                    break "interrupted";
                }
                if !self.pending_tool_steering.is_empty() || ui.pending_input() {
                    break "user_input";
                }
                if self.stop_boundary().is_some() {
                    break "stop_boundary";
                }
                self.pump_subagents(ui);
                if self.background_result_ready() {
                    break "result_or_message";
                }
                // The wait parks the turn at a request boundary, so mail can be surfaced right away.
                if self.peer_mail_ready() {
                    break "peer_message";
                }
                if self.waitable_background_jobs().is_empty()
                    && self.live_tasks.running_ids().is_empty()
                    && self.side_questions.running_ids().is_empty()
                {
                    // Work may have finished after the pump above; deliver that result, not a bare
                    // "no longer running" note. A job's completion notification can trail its exit
                    // briefly, so give it a bounded grace period before falling back.
                    let grace = Instant::now() + SETTLE_GRACE;
                    let mut ready = false;
                    loop {
                        self.pump_subagents(ui);
                        if self.background_result_ready() {
                            ready = true;
                            break;
                        }
                        if Instant::now() >= grace || self.interrupt.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(SETTLE_POLL);
                    }
                    if ready {
                        break "result_or_message";
                    }
                    // A provider may settle work without a notification. Never turn its disappearance
                    // into proof of success; the next request must inspect the actual result.
                    self.background_notices.push("[Awaited work is no longer running] Inspect its terminal results before reporting success; the end of a wait is not verification.".into());
                    break "settled";
                }
                if let Some(bound) = job_bound
                    && Instant::now() >= bound
                {
                    let running = self.waitable_background_jobs();
                    if !running.is_empty() {
                        self.unattended_wait_noticed.extend(running.iter().cloned());
                        self.background_notices
                            .push(unattended_running_notice(&running));
                        break "unattended_bound";
                    }
                }
                let mut timeout = self.modes.launch.deadline.map_or(HOST_TICK, |deadline| {
                    HOST_TICK.min(deadline.saturating_duration_since(Instant::now()))
                });
                if let Some(bound) = job_bound {
                    timeout = timeout.min(bound.saturating_duration_since(Instant::now()));
                }
                waiter.wait::<std::io::PipeReader>(None, None, Some(timeout))?;
            })
        })();
        let reason = result.as_ref().copied().unwrap_or("wait_failed");
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        ui.event(
            &UiEvent::new(
                EventKind::Wait,
                format!(
                    "Work wait ended: {reason}. Inspect delivered results before reporting success."
                ),
            )
            .with_background_wait(BackgroundWait::Ended {
                reason: reason.into(),
                elapsed_ms,
            }),
        );
        self.record(
            self.cur_step,
            "background_wait_ended",
            json!({
                "reason":reason,"elapsed_ms":started.elapsed().as_millis()
            }),
        )?;
        result.map(|_| true)
    }

    fn background_result_ready(&mut self) -> bool {
        !self.background_notices.is_empty()
            || !self.live_tasks.undelivered().is_empty()
            || self.side_questions.has_undelivered()
    }

    /// Awaited `bash` jobs, minus those an unattended run already waited on for its bound.
    pub(in crate::agent) fn waitable_background_jobs(&self) -> Vec<String> {
        let mut jobs = self.awaited_background_jobs();
        if self.modes.launch.unattended {
            jobs.retain(|job| !self.unattended_wait_noticed.contains(job));
        }
        jobs
    }

    /// Running awaited jobs the caller explicitly sent to the background.
    fn requested_background_jobs(&self) -> Vec<String> {
        let mut jobs = Vec::new();
        for name in self.ext.names() {
            if let Some(tool) = self.ext.get(name) {
                jobs.extend(tool.requested_background_work());
            }
        }
        jobs
    }
}

fn unattended_running_notice(running: &[String]) -> String {
    format!(
        "[Unattended run: background work is still running]\n{}\nNo user can steer this run, so Dial stopped waiting for it. If a server or service must stay up after you finish, stop it and start it with `bg` action=start persist=true name=<name> (without `ready`), then confirm it serves. If you need a result, use a bounded `bg wait`. Otherwise stop it with `bg kill`. Later completion waits skip these jobs.",
        running.join("\n")
    )
}

#[cfg(test)]
mod tests;

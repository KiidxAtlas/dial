//! Suspend inference at a yield boundary until owned work has something useful to deliver.
use crate::agent::*;

// Legacy hosts may queue input without signalling the platform wake channel. Keep their input,
// paint, runtime projection and budget checks responsive at the UI heartbeat, but never ask the
// model to poll on this tick.
const HOST_TICK: Duration = crate::agent::ui::chat::HEARTBEAT;

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
        let jobs = self.awaited_background_jobs();
        let workers = self.live_tasks.running_ids();
        let questions = self.side_questions.running_ids();
        if jobs.is_empty() && workers.is_empty() && questions.is_empty() {
            // Listing the jobs refreshes their table, so one may have finished since the pump above.
            self.pump_subagents(ui);
            return Ok(self.background_result_ready());
        }
        let started = Instant::now();
        self.record(
            self.cur_step,
            "background_wait_started",
            json!({
                "jobs":jobs,"workers":workers,"side_questions":questions
            }),
        )?;
        ui.notice(&format!(
            "Waiting for running work without model polling: {}. New input interrupts the wait.",
            jobs.iter()
                .chain(&workers)
                .chain(&questions)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let reason = loop {
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
            if self.awaited_background_jobs().is_empty()
                && self.live_tasks.running_ids().is_empty()
                && self.side_questions.running_ids().is_empty()
            {
                // Work may have finished after the pump above; deliver that result, not a bare
                // "no longer running" note.
                self.pump_subagents(ui);
                if self.background_result_ready() {
                    break "result_or_message";
                }
                // A provider may settle work without a notification. Never turn its disappearance
                // into proof of success; the next request must inspect the actual result.
                self.background_notices.push("[Awaited work is no longer running] Inspect its terminal results before reporting success; the end of a wait is not verification.".into());
                break "settled";
            }
            let timeout = self.modes.launch.deadline.map_or(HOST_TICK, |deadline| {
                HOST_TICK.min(deadline.saturating_duration_since(Instant::now()))
            });
            waiter.wait::<std::io::PipeReader>(None, None, Some(timeout))?;
        };
        self.record(
            self.cur_step,
            "background_wait_ended",
            json!({
                "reason":reason,"elapsed_ms":started.elapsed().as_millis()
            }),
        )?;
        Ok(true)
    }

    fn background_result_ready(&mut self) -> bool {
        !self.background_notices.is_empty()
            || !self.live_tasks.undelivered().is_empty()
            || self.side_questions.has_undelivered()
    }
}

#[cfg(test)]
mod tests;

//! Warn-cost notices for `[budget] warn_session_cost_usd` and `warn_turn_cost_usd`. A warning
//! notifies once per crossing and never stops work; the hard caps (`max_*_cost_usd`) keep their
//! stop boundary in `begin.rs`. Session spend is the shared ledger, so subagent cost counts.
use crate::agent::*;

/// Tracks one spend level against a warn threshold. Armed while spend is below the threshold; the
/// first observation at or above it disarms it and reports the crossing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) struct Crossing {
    armed: bool,
}

impl Crossing {
    pub(in crate::agent) fn new(armed: bool) -> Self {
        Self { armed }
    }

    /// Records `spent` against `threshold`. Returns true exactly when spend has moved from below to
    /// at or above the threshold since it was last observed.
    pub(in crate::agent) fn observe(&mut self, spent: f64, threshold: f64) -> bool {
        if spent < threshold {
            self.armed = true;
            false
        } else {
            std::mem::replace(&mut self.armed, false)
        }
    }
}

impl Agent<'_> {
    /// Notifies once per warn-threshold crossing: the session total (which includes subagents) and
    /// the current turn. Runs at each step boundary and after each reply so a crossing is seen
    /// before the turn can end. Thresholds are read from the live configuration on every call.
    pub(in crate::agent) fn observe_cost_warnings(
        &mut self,
        turn_warn: &mut Crossing,
        ui: &mut dyn Ui,
    ) {
        if self.hermetic {
            return;
        }
        let session_threshold = self
            .config
            .budget
            .warn_session_cost_usd
            .filter(|value| *value > 0.0);
        let turn_threshold = self
            .config
            .budget
            .warn_turn_cost_usd
            .filter(|value| *value > 0.0);
        if let Some(threshold) = session_threshold
            && let Some(spent) = self.session_cost()
        {
            // A session resumed already past its threshold was warned in the run that crossed it.
            let crossing = self
                .session_cost_warn
                .get_or_insert_with(|| Crossing::new(spent < threshold));
            if crossing.observe(spent, threshold) {
                ui.notice(&format!(
                    "Session cost ${spent:.4} reached the ${threshold:.4} warning threshold (budget.warn_session_cost_usd). Work continues; budget.max_session_cost_usd is the stop boundary."
                ));
            }
        }
        if let Some(threshold) = turn_threshold
            && let Some(spent) = self.usage_scope.observed_turn_cost()
            && turn_warn.observe(spent, threshold)
        {
            ui.notice(&format!(
                "Turn cost ${spent:.4} reached the ${threshold:.4} warning threshold (budget.warn_turn_cost_usd). Work continues; budget.max_turn_cost_usd is the stop boundary."
            ));
        }
    }
}

#[cfg(test)]
mod tests;

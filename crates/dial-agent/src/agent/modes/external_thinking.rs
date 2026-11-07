//! `--external-thinking`: the model reasons in a private scratchpad the user never sees.
//! Mechanism: provider reasoning is forced off on each request (`ReasoningEffort::None`, applied
//! after any other effort choice, and automatic thinking selection is skipped); `INSTRUCTION` is
//! restated per request in the transient `session_mode` notice (`Agent::mode_notice`), so the
//! cached system prefix never changes; and `<scratchpad>` blocks are removed by `strip_scratchpad`
//! from replies and stalled partials before they are shown or stored.

use crate::agent::Agent;
pub use crate::agent::prompts::modes::external_thinking::INSTRUCTION;

const OPEN: &str = "<scratchpad>";
const CLOSE: &str = "</scratchpad>";

/// Removes every `<scratchpad>...</scratchpad>` block. An unterminated block hides the rest of the
/// reply, because the scratchpad is private by definition.
pub fn strip_scratchpad(text: &str) -> String {
    if !text.contains(OPEN) {
        return text.to_owned();
    }
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        kept.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

impl Agent<'_> {
    /// The scratchpad instruction for `--external-thinking`, or `None` when the flag is off. It is
    /// carried by the request-tail mode notice, not the system prompt.
    pub fn external_thinking_instruction(&self) -> Option<&'static str> {
        self.modes.launch.external_thinking.then_some(INSTRUCTION)
    }

    /// `/external-thinking on|off`: the same per-process switch as `--external-thinking`, never
    /// persisted. The next request's mode notice and reasoning effort follow the new value; the
    /// rebuild keeps any other derived prompt state current but leaves an unchanged prefix as is.
    pub fn set_external_thinking(&mut self, enabled: bool) {
        if self.modes.launch.external_thinking != enabled {
            self.modes.launch.external_thinking = enabled;
            self.rebuild_system();
        }
    }

    pub fn external_thinking_enabled(&self) -> bool {
        self.modes.launch.external_thinking
    }
}

#[cfg(test)]
mod tests;

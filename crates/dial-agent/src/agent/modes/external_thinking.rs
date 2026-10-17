//! `--external-thinking`: the model reasons in a private scratchpad the
//! user never sees. Mechanism: provider reasoning is forced off through the thinking level
//! (`ReasoningEffort::None`), `INSTRUCTION` is added to the system prompt, and `<scratchpad>`
//! blocks are removed by `strip_scratchpad` before the reply is shown or stored.

use crate::agent::Agent;

/// System-prompt addition that routes private reasoning into one `<scratchpad>` block.
pub const INSTRUCTION: &str = "Reason privately before answering. Put all scratch reasoning inside one <scratchpad>...</scratchpad> block at the start of your reply. Text outside that block is shown to the user.";

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
    /// The system-prompt addition for `--external-thinking`, or `None` when the flag is off.
    pub fn external_thinking_instruction(&self) -> Option<&'static str> {
        self.modes.launch.external_thinking.then_some(INSTRUCTION)
    }

    /// `/external-thinking on|off`: the same per-process switch as `--external-thinking`, never
    /// persisted. The scratchpad instruction lives in the system prompt, so it is rebuilt now and
    /// the next request carries the change.
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

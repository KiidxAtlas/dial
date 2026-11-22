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

    /// Strip private `<scratchpad>` blocks when `--external-thinking` is active; otherwise return
    /// the text unchanged. Single normalization entry point for replies, partials and `finish`.
    pub(in crate::agent) fn normalize_visible_text(&self, text: &str) -> String {
        if self.external_thinking_enabled() {
            strip_scratchpad(text)
        } else {
            text.to_owned()
        }
    }

    /// Normalize a received provider reply at ingestion and emit visible text when streaming was
    /// withheld for scratchpad filtering.
    pub(in crate::agent) fn normalize_reply(
        &self,
        reply: &mut crate::llm::Reply,
        stream_visible: bool,
        ui: &mut dyn crate::agent::Ui,
    ) {
        if !self.external_thinking_enabled() {
            return;
        }
        reply.text = strip_scratchpad(&reply.text);
        if stream_visible && !reply.text.is_empty() {
            ui.text_delta(&reply.text);
        }
    }

    /// Strip `<scratchpad>` blocks from every user-visible field of a typed `finish` outcome.
    pub(in crate::agent) fn normalize_finish(
        &self,
        finish: &mut crate::agent::verify::completion::Finish,
    ) {
        if !self.external_thinking_enabled() {
            return;
        }
        finish.summary = strip_scratchpad(&finish.summary);
        finish.text = strip_scratchpad(&finish.text);
        if let Some(question) = finish.question.as_mut() {
            *question = strip_scratchpad(question);
        }
    }
}

#[cfg(test)]
mod tests;

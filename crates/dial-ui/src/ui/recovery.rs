//! Plain wording for the agent's "needs direction" stop. The raw message is the model-facing
//! diagnostic (for example "Step not closed: no tool has run since…"); the panel shows [`summary`]
//! and reveals the raw text only when the user asks for it.

/// One plain line for a raw stop message. Pure: the caller passes the text it would otherwise show.
pub fn summary(raw: &str) -> &'static str {
    if raw.contains("Step not closed") {
        "The agent stopped because a step was not finished. Ask it to continue or describe the next step."
    } else {
        "The agent stopped before finishing. Ask it to continue or describe the next step."
    }
}

/// The text the panel body shows: the summary, or the raw message once the user has asked for it.
pub fn panel_text(raw: &str, show_raw: bool) -> &str {
    if show_raw { raw } else { summary(raw) }
}

#[cfg(test)]
mod tests;

//! What a provider error usually means and the one step that most often fixes it. The error card
//! names the kind on its top edge and the step on its bottom edge; the message itself is unchanged.
//! Matching is by the words and status codes providers put in their errors, so an error nothing
//! here recognizes keeps the plain card rather than a guessed hint.

/// `(kind, next step)` for an error message, or `None` when it is not one of the common cases.
pub(super) fn classify(message: &str) -> Option<(&'static str, &'static str)> {
    let m = message.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| m.contains(w));
    let status = |code: &str| {
        m.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| word == code)
    };
    if status("429")
        || has(&[
            "rate limit",
            "rate_limit",
            "too many requests",
            "quota exceeded",
        ])
    {
        return Some((
            "rate limited",
            "wait a moment and send again, or switch models with /model",
        ));
    }
    if status("401")
        || status("403")
        || has(&[
            "unauthorized",
            "invalid api key",
            "invalid x-api-key",
            "incorrect api key",
            "authentication",
            "api key not",
            "missing api key",
            "permission denied by the provider",
        ])
    {
        return Some((
            "not signed in",
            "check the provider's key with /providers or /login",
        ));
    }
    if has(&[
        "context length",
        "context_length",
        "maximum context",
        "too many tokens",
        "prompt is too long",
        "context window",
    ]) {
        return Some((
            "context too long",
            "shrink history with /compact or start fresh with /new",
        ));
    }
    if has(&[
        "model not found",
        "model_not_found",
        "no such model",
        "unknown model",
        "does not exist",
    ]) && m.contains("model")
    {
        return Some(("model not found", "pick another model with /model"));
    }
    if status("529")
        || status("503")
        || status("502")
        || has(&["overloaded", "service unavailable", "bad gateway"])
    {
        return Some((
            "provider busy",
            "send again shortly, or switch models with /model",
        ));
    }
    if has(&[
        "timed out",
        "timeout",
        "connection refused",
        "connection reset",
        "dns error",
        "failed to lookup address",
        "network is unreachable",
    ]) {
        return Some(("network problem", "check your connection, then send again"));
    }
    None
}

#[cfg(test)]
mod tests;

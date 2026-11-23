//! Unified prose-signal classifiers for plain assistant replies.
//!
//! Plain reports can declare a terminal status or ask a question. Closing-line hints serve
//! bounded recovery; they do not establish a delivery obligation or a verification receipt.

use crate::agent::verify::completion::TurnOutcome;

/// Normalized summary of prose-level completion signals on a plain reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) struct ReplySignals {
    pub declared: Option<TurnOutcome>,
    pub awaiting_input: bool,
    pub announces_unexpected_stop: bool,
    pub reports_unfinished_work: bool,
}

impl ReplySignals {
    pub(in crate::agent) fn classify(text: &str) -> Self {
        Self {
            declared: declared_outcome(text),
            awaiting_input: awaiting_input(text),
            announces_unexpected_stop: announces_unexpected_stop(text),
            reports_unfinished_work: reports_unfinished_work(text),
        }
    }

    /// True when the reply's closing line announces an immediate next action or unfinished work.
    pub(in crate::agent) fn unfinished_closing(&self) -> bool {
        self.announces_unexpected_stop || self.reports_unfinished_work
    }
}

/// Normalize prose for conservative lexical matching: ASCII/Unicode lowercase and curly
/// apostrophe folded to straight ASCII `'`.
fn normalize_prose(text: &str) -> String {
    text.to_lowercase().replace('\u{2019}', "'")
}

/// Whether `text` ends inside an unclosed fenced code block.
fn has_unclosed_fence(text: &str) -> bool {
    text.lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count()
        % 2
        != 0
}

/// The normalized closing prose line when `text` does not end inside an unclosed fence and the
/// last line is not quoted, bulleted, or backtick-prefixed code.
fn closing_prose_line(text: &str) -> Option<String> {
    let last = text.trim().lines().last().unwrap_or("").trim();
    if last.is_empty()
        || last.starts_with(['`', '"', '“', '>', '-', '*'])
        || has_unclosed_fence(text)
    {
        return None;
    }
    Some(normalize_prose(last))
}

/// Explicit labels in plain reports use the same outcomes as typed finish calls.
pub(in crate::agent) fn declared_outcome(text: &str) -> Option<TurnOutcome> {
    let first = text
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if first.starts_with("blocked:") || first.starts_with("blocked on ") {
        Some(TurnOutcome::Blocked)
    } else if first.starts_with("incomplete:") {
        Some(TurnOutcome::Incomplete)
    } else if first.starts_with("failed:") {
        Some(TurnOutcome::Failed)
    } else if first.starts_with("needs input:") {
        Some(TurnOutcome::NeedsInput)
    } else {
        None
    }
}

/// Recognize a plain final reply that asks the user something: an explicit request for clarification
/// ("could you clarify ...") or a decision question ("Which branch should I use?", "Do you want ...?",
/// "Would you like ...?"). Only the final paragraph counts, and it must end with a question mark and
/// not sit inside code. Rhetorical questions ("What changed?") and questions followed by a statement
/// do not match. Open todos must survive it.
pub(in crate::agent) fn awaiting_input(text: &str) -> bool {
    if has_unclosed_fence(text) {
        return false;
    }
    let last = normalize_prose(text.trim().rsplit("\n\n").next().unwrap_or("").trim());
    if !last.ends_with('?') || last.contains("```") {
        return false;
    }
    [
        "could you clarify",
        "can you clarify",
        "could you provide",
        "can you provide",
        "could you confirm",
        "can you confirm",
        "which option would you",
        "which would you like",
        "what would you like me to",
        "should i proceed with",
    ]
    .iter()
    .any(|prefix| last.starts_with(prefix))
        || last.contains("should i")
        || last.contains("should we")
        || ["do you ", "would you ", "shall i "]
            .iter()
            .any(|prefix| last.starts_with(prefix))
}

/// Whether an answer says a capability is unavailable to the agent itself ("I cannot provide an
/// interactive terminal", "a browser is not available to me"): one sentence pairs a first-person
/// subject or object with a denial and a capability noun, so prose about the project ("this env
/// var is not available in tests") does not match. Code is ignored. A heuristic for one search
/// reminder per turn, never a reason to reject an answer outright.
pub(in crate::agent) fn denies_capability(text: &str) -> bool {
    const FIRST_PERSON: &[&str] = &["i", "i'm", "im", "i've", "me", "my", "myself"];
    const DENIALS: &[&str] = &[
        "cannot",
        "can't",
        "can not",
        "unable to",
        "not available",
        "isn't available",
        "don't have",
        "do not have",
        "no access",
        "not possible",
        "isn't possible",
        "not supported",
        "lack ",
    ];
    const CAPABILITIES: &[&str] = &[
        "tool",
        "environment",
        "terminal",
        "tty",
        "access to",
        "capabilit",
        "ability",
        "interactive",
        "browser",
        "internet",
        "network",
        "display",
        "screen",
        "gui",
    ];
    text.split("```")
        .step_by(2)
        .flat_map(|prose| prose.split(['.', '!', '?', '\n']))
        .map(normalize_prose)
        .any(|sentence| {
            sentence
                .split(|c: char| !c.is_alphanumeric() && c != '\'')
                .any(|word| FIRST_PERSON.contains(&word))
                && DENIALS.iter().any(|denial| sentence.contains(denial))
                && CAPABILITIES
                    .iter()
                    .any(|capability| sentence.contains(capability))
        })
}

/// Conservatively recognize an action-only final sentence. Quotes, code and optional future
/// work are reports, not a reason to issue another model request.
pub(crate) fn announces_unexpected_stop(text: &str) -> bool {
    const INTENT: [&str; 8] = [
        "let me ",
        "i'll ",
        "i will ",
        "i'm going to ",
        "next, i'll ",
        "next, i will ",
        "now i'll",
        "now let me",
    ];
    let Some(lower) = closing_prose_line(text) else {
        return false;
    };
    let sentence = lower.rsplit(". ").next().unwrap_or(&lower).trim();
    // Closing offers ("Let me know what you'd like to change.") hand the turn back to the user;
    // they are not announced actions.
    if [
        "optional",
        "later",
        "next time",
        "if you",
        "when you",
        "leave ",
        "can help",
        "let me know",
        "would you like",
        "want me to",
        "happy to",
        "blocked",
        "cannot",
        "can't",
        "unable",
    ]
    .iter()
    .any(|phrase| sentence.contains(phrase))
    {
        return false;
    }
    let sentence = sentence.strip_prefix("plan: ").unwrap_or(sentence);
    // A dangling transition introduces work or output that never followed. Match the
    // whole last line: it may contain several sentences describing the upcoming edit.
    if lower.ends_with(':')
        && ["now the ", "next, the ", "next the "]
            .iter()
            .any(|p| lower.starts_with(p))
        && ![" is ", " are ", " has ", " have "]
            .iter()
            .any(|p| lower.contains(p))
    {
        return true;
    }
    let action = sentence
        .rsplit_once(", so ")
        .map_or(sentence, |(_, action)| action);
    let present = action
        .strip_prefix("i'm ")
        .or_else(|| action.strip_prefix("i am "));
    let announced = present.unwrap_or(action);
    if (present.is_some()
        || action.ends_with(':')
        || action.trim_end_matches(['.', '!']).ends_with(" now"))
        && [
            "writing ",
            "rewriting ",
            "replacing ",
            "updating ",
            "editing ",
            "implementing ",
            "checking ",
            "running ",
            "inspecting ",
            "reading ",
            "fixing ",
            "testing ",
            "building ",
            "verifying ",
        ]
        .iter()
        .any(|verb| announced.starts_with(verb))
    {
        return true;
    }
    INTENT.iter().any(|prefix| sentence.starts_with(prefix))
        && [
            "check",
            "run",
            "inspect",
            "read",
            "edit",
            "update",
            "implement",
            "fix",
            "trace",
            "test",
            "search",
            "open",
            "continue",
            "verify",
            "build",
            "change",
            "write",
            "replace",
            "create",
        ]
        .iter()
        .any(|verb| {
            sentence
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word == *verb)
        })
}

/// Explicit admission that the requested work is still underway, without reading quoted examples
/// or optional follow-ups as instructions. This shares the ordinary reply-recovery budget.
pub(crate) fn reports_unfinished_work(text: &str) -> bool {
    let Some(lower) = closing_prose_line(text) else {
        return false;
    };
    if ["blocked", "optional", "if you", "when you", "later"]
        .iter()
        .any(|word| lower.contains(word))
    {
        return false;
    }
    // First-person subjects only: "you still need to set the token" hands a step to the user
    // and is a finished answer.
    if [
        "still pending",
        "work is pending",
        "creation is still pending",
        "still open",
        "working on it",
        "i will get started",
        "i have not read the source",
        "i have not read the",
        "i have not run the",
        "i haven't read the",
        "i haven't run the",
        "i still need to",
        "i still have to",
        "we still need to",
        "still need to deliver",
        "i haven't finished",
        "i have not finished",
        "isn't finished yet",
        "is not finished yet",
        "not done yet",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
    {
        return true;
    }
    // "Next I'd update the parser" names the next step of unfinished work; "Next I'd suggest
    // adding a test" is a recommendation that closes a finished one.
    ["next i'd ", "next, i'd ", "next i would ", "next, i would "]
        .iter()
        .filter_map(|prefix| lower.find(prefix).map(|at| &lower[at + prefix.len()..]))
        .any(|rest| {
            ![
                "suggest",
                "recommend",
                "advise",
                "propose",
                "consider",
                "like",
                "love",
            ]
            .iter()
            .any(|word| rest.starts_with(word))
        })
}

#[cfg(test)]
mod tests;

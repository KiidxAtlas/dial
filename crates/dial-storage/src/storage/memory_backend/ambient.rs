//! Admission for optional automatic recall; explicit search keeps its broader vocabulary.
use std::collections::BTreeSet;

pub fn is_term(term: &str) -> bool {
    !matches!(
        term,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "that"
            | "this"
            | "where"
            | "how"
            | "does"
            | "which"
            | "into"
            | "are"
            | "when"
            | "implement"
            | "implementation"
            | "can"
            | "could"
            | "would"
            | "you"
            | "your"
            | "ur"
            | "our"
            | "we"
            | "my"
            | "all"
            | "these"
            | "those"
            | "them"
            | "yes"
            | "no"
            | "please"
            | "thanks"
            | "what"
            | "was"
            | "were"
            | "it"
            | "its"
            | "at"
            | "of"
            | "to"
            | "in"
            | "if"
            | "or"
            | "not"
            | "have"
            | "has"
            | "had"
            | "be"
            | "been"
            | "anything"
            | "something"
            | "specific"
            | "current"
            | "look"
            | "check"
            | "fix"
            | "make"
            | "sure"
            | "should"
            | "shouldnt"
            | "really"
            | "properly"
            | "continue"
    )
}

fn terms(text: &str) -> BTreeSet<String> {
    super::semantic_store::query_tokens(text)
        .into_iter()
        .filter(|term| is_term(term))
        .collect()
}

/// A short subject can match one word. Long prompts need several independent content matches;
/// lexical admission is deliberately conservative even when a backend also ranks by embeddings.
/// Explicit recall remains available for synonyms and broader historical questions.
pub fn relevant(query: &str, content: &str) -> bool {
    let query_terms = terms(query);
    if query_terms.is_empty() {
        return false;
    }
    let required = if super::semantic_store::query_tokens(query).len() > 8 {
        query_terms.len().clamp(2, 3)
    } else {
        1
    };
    terms(content).intersection(&query_terms).count() >= required
}

/// Synthetic messages may carry role=user for provider compatibility, but are not recall subjects.
pub(super) fn user_message(message: &serde_json::Value) -> bool {
    message["role"] == "user"
        && [
            "dial_transient",
            "dial_portable_rules",
            "dial_context_notes",
            "dial_context_rollover",
            "dial_worker_result",
            "dial_side_result",
            "dial_reminder",
            "dial_exploration_reminder",
            "dial_compaction_critical",
            "dial_generated_user",
            "dial_compaction_summary",
            "dial_compaction_notes",
            "dial_branch_summary",
            "dial_continuation",
            "dial_snapcompact",
        ]
        .iter()
        .all(|key| {
            message
                .get(*key)
                .is_none_or(|value| value.is_null() || *value == false)
        })
}

/// Conversational continuations may inherit the most recent real user subject, not arbitrary
/// assistant prose or tool logs. A new substantive user message always replaces that subject.
pub fn query<'a>(latest: &'a str, history: &'a [serde_json::Value]) -> &'a str {
    if !terms(latest).is_empty() {
        return latest;
    }
    history
        .iter()
        .rev()
        .find_map(|message| {
            if !user_message(message) {
                return None;
            }
            message["content"]
                .as_str()
                .filter(|text| !terms(text).is_empty())
        })
        .unwrap_or(latest)
}

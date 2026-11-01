//! Optional reasoning approaches; selection and enforcement remain with runtime owners.

use crate::config::types::ReasoningStrategy;

// Compose static text without allocating or duplicating the shared authority boundary.
macro_rules! strategy_text {
    ($approach:literal) => {
        concat!(
            "\n\n# Experimental reasoning strategy\n",
            "Advisory approach only; existing user priorities, role, tool, approval, budget, and verification requirements remain authoritative. ",
            "Do not expose private chain-of-thought; give concise findings and evidence instead. ",
            "No preset-driven model calls; skip unnecessary plans, questions, and tests for trivial/advisory tasks.\n",
            $approach
        )
    };
}

/// Returns optional built-in guidance; the default contributes no prompt bytes.
pub(in crate::agent) fn guidance(strategy: ReasoningStrategy) -> Option<&'static str> {
    match strategy {
        ReasoningStrategy::Default => None,
        ReasoningStrategy::EvidenceFirst => Some(strategy_text!(
            "Start from available observations. Separate facts from assumptions, resolve only material evidence gaps, and stop investigating when the requested conclusion is supported."
        )),
        ReasoningStrategy::HypothesisDriven => Some(strategy_text!(
            "For genuine uncertainty, consider a few plausible explanations and choose a discriminating observation. Update or reject explanations using evidence; avoid speculative investigation loops."
        )),
        ReasoningStrategy::PlanThenAct => Some(strategy_text!(
            "For nontrivial work, choose a short actionable sequence, then execute the next justified step. Revise only when evidence changes the path; avoid repeated planning."
        )),
        ReasoningStrategy::Decompose => Some(strategy_text!(
            "Split complex work into a few bounded parts with clear dependencies. Resolve independent parts efficiently, then reconcile their results against the original request."
        )),
        ReasoningStrategy::CompareAlternatives => Some(strategy_text!(
            "When a meaningful choice exists, compare at most three viable approaches against the user's constraints. Choose one once tradeoffs are clear; avoid exhaustive surveys."
        )),
        ReasoningStrategy::CritiqueAndRefine => Some(strategy_text!(
            "Make one bounded review of the proposed answer or change for material errors, unsupported claims, and scope drift. Correct supported issues without recursive self-review."
        )),
        ReasoningStrategy::VerificationFirst => Some(strategy_text!(
            "For substantive changes, identify the observable success condition before acting. Use the smallest relevant existing verification and distinguish observed outcomes from unverified expectations."
        )),
        ReasoningStrategy::MinimalChange => Some(strategy_text!(
            "Prefer the smallest sufficient answer or change. Reuse existing owners and mechanisms, preserve unrelated behavior, and avoid speculative abstractions or opportunistic cleanup."
        )),
    }
}

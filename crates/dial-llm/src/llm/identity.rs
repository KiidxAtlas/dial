//! Central model identity. Each behavioral consumer is an [`Axis`] with its own declarative rule
//! table, because the same name legitimately means different things to different payloads: the loop
//! detectors trust only a leading model family, the Devin payload also trusts a substring, and the
//! Cloud Code Assist Claude label trusts the leading `claude` prefix exactly as written. Callers never
//! match model strings; they ask an axis for a family.
//!
//! Precedence is declaration order within an axis. Authoritative upstream evidence (a uid) is
//! declared before name heuristics, so a uid outranks a conflicting name. A name matching several
//! families is not an error: the first declared rule decides. No match resolves to `None`.

/// A behavioral family. Families group behavior, not capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Claude,
    DeepSeek,
    Gemini,
    Xai,
}

/// A consumer that needs a family. Each axis has its own rule table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Stream loop detectors. Trusts only the lowercased final path segment starting with a family.
    LoopDetectors,
    /// Devin chat payload. Trusts the upstream uid, or the lowercased name containing `gemini`.
    DevinPayload,
    /// Cloud Code Assist Antigravity Claude label. Trusts the exact leading `claude` prefix.
    CloudCodeClaude,
}

#[derive(Debug, Clone, Copy)]
enum Evidence {
    /// Lowercased final path segment starts with the prefix.
    Basename(&'static str),
    /// Lowercased full name contains the needle.
    Contains(&'static str),
    /// Name as written starts with the prefix (case-sensitive).
    Leading(&'static str),
    /// Upstream uid as written starts with the prefix (case-sensitive). Authoritative.
    Uid(&'static str),
}

#[derive(Debug, Clone, Copy)]
struct Rule {
    family: Family,
    evidence: Evidence,
}

const LOOP_DETECTORS: &[Rule] = &[
    Rule {
        family: Family::Gemini,
        evidence: Evidence::Basename("gemini"),
    },
    Rule {
        family: Family::DeepSeek,
        evidence: Evidence::Basename("deepseek"),
    },
    Rule {
        family: Family::Xai,
        evidence: Evidence::Basename("grok"),
    },
    Rule {
        family: Family::Xai,
        evidence: Evidence::Basename("xai"),
    },
];

const DEVIN_PAYLOAD: &[Rule] = &[
    Rule {
        family: Family::Gemini,
        evidence: Evidence::Uid("MODEL_GOOGLE_GEMINI_"),
    },
    Rule {
        family: Family::Gemini,
        evidence: Evidence::Contains("gemini"),
    },
];

const CLOUD_CODE_CLAUDE: &[Rule] = &[Rule {
    family: Family::Claude,
    evidence: Evidence::Leading("claude"),
}];

impl Axis {
    fn rules(self) -> &'static [Rule] {
        match self {
            Self::LoopDetectors => LOOP_DETECTORS,
            Self::DevinPayload => DEVIN_PAYLOAD,
            Self::CloudCodeClaude => CLOUD_CODE_CLAUDE,
        }
    }
}

impl Evidence {
    fn matches(self, name: &str, lower: &str, basename: &str, uid: Option<&str>) -> bool {
        match self {
            Self::Basename(prefix) => basename.starts_with(prefix),
            Self::Contains(needle) => lower.contains(needle),
            Self::Leading(prefix) => name.starts_with(prefix),
            Self::Uid(prefix) => uid.is_some_and(|uid| uid.starts_with(prefix)),
        }
    }
}

/// The identity inputs for one request: the model name and an optional upstream uid.
#[derive(Debug, Clone, Copy)]
pub struct Identity<'a> {
    name: &'a str,
    uid: Option<&'a str>,
}

impl<'a> Identity<'a> {
    pub fn new(name: &'a str) -> Self {
        Self { name, uid: None }
    }

    pub fn with_uid(self, uid: &'a str) -> Self {
        Self {
            uid: Some(uid),
            ..self
        }
    }

    /// The family the first matching rule of `axis` declares, or `None` when nothing matches.
    pub fn family(self, axis: Axis) -> Option<Family> {
        let lower = self.name.to_ascii_lowercase();
        let basename = lower.rsplit('/').next().unwrap_or(&lower);
        axis.rules()
            .iter()
            .find(|rule| rule.evidence.matches(self.name, &lower, basename, self.uid))
            .map(|rule| rule.family)
    }
}

#[cfg(test)]
mod tests;

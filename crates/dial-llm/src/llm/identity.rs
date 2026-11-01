//! Central model identity. Each behavioral consumer is an [`Axis`] with its own declarative rule
//! table, because the same name legitimately means different things to different payloads: the loop
//! detectors trust only a leading model family, the Devin payload also trusts a substring, and the
//! Cloud Code Assist Claude label trusts the leading `claude` prefix exactly as written. Callers never
//! match model strings; they ask an axis for a family or a named predicate.
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
    Glm,
    Gpt,
    Kimi,
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
    /// Snapcompact layout and detail policy. Trusts the lowercased final path segment starting with
    /// a model family name (gateway prefixes such as `openrouter/anthropic/` are ignored).
    Snapcompact,
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

const SNAPCOMPACT: &[Rule] = &[
    Rule {
        family: Family::Claude,
        evidence: Evidence::Basename("claude-"),
    },
    Rule {
        family: Family::Gemini,
        evidence: Evidence::Basename("gemini-"),
    },
    Rule {
        family: Family::Gpt,
        evidence: Evidence::Basename("gpt-"),
    },
    Rule {
        family: Family::Gpt,
        evidence: Evidence::Basename("codex-"),
    },
    Rule {
        family: Family::Kimi,
        evidence: Evidence::Basename("kimi-"),
    },
    Rule {
        family: Family::Glm,
        evidence: Evidence::Basename("glm-"),
    },
];

impl Axis {
    fn rules(self) -> &'static [Rule] {
        match self {
            Self::LoopDetectors => LOOP_DETECTORS,
            Self::DevinPayload => DEVIN_PAYLOAD,
            Self::CloudCodeClaude => CLOUD_CODE_CLAUDE,
            Self::Snapcompact => SNAPCOMPACT,
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
        let basename = basename_of(&lower);
        let basename = if axis == Axis::Snapcompact {
            claude_leaf(basename)
        } else {
            basename
        };
        axis.rules()
            .iter()
            .find(|rule| rule.evidence.matches(self.name, &lower, basename, self.uid))
            .map(|rule| rule.family)
    }

    /// The Snapcompact family (see [`Axis::Snapcompact`]).
    pub fn snapcompact_family(self) -> Option<Family> {
        self.family(Axis::Snapcompact)
    }

    /// Claude Opus 4.7 or later, for example `claude-opus-4-7` or `anthropic/claude-opus-4.7-20260101`.
    pub fn opus_4_7_or_later(self) -> bool {
        let leaf = self.leaf();
        version_after(claude_leaf(&leaf), "claude-opus-").is_some_and(|version| version >= (4, 7))
    }

    /// The Claude codename families `fable` and `mythos` (`claude-mythos-…`).
    pub fn fable_or_mythos(self) -> bool {
        let leaf = self.leaf();
        claude_leaf(&leaf)
            .strip_prefix("claude-")
            .and_then(|rest| rest.split(['-', ':', '@']).next())
            .is_some_and(|codename| matches!(codename, "fable" | "mythos"))
    }

    /// GPT-5.4 or later, including Codex models (`gpt-5.4`, `gpt-5-4-mini`, `codex-5.4`).
    pub fn gpt_5_4_or_later(self) -> bool {
        let leaf = self.leaf();
        ["gpt-", "codex-"]
            .iter()
            .any(|stem| version_after(&leaf, stem).is_some_and(|version| version >= (5, 4)))
    }

    fn leaf(self) -> String {
        let lower = self.name.to_ascii_lowercase();
        basename_of(&lower).to_owned()
    }
}

/// The final path segment of a lowercased model name (gateway prefixes are dropped).
fn basename_of(lower: &str) -> &str {
    lower.rsplit('/').next().unwrap_or(lower)
}

/// Bedrock vendor and cross-region prefixes are not part of the Claude model id.
fn claude_leaf(leaf: &str) -> &str {
    [
        "anthropic.",
        "us.anthropic.",
        "eu.anthropic.",
        "apac.anthropic.",
        "global.anthropic.",
    ]
    .iter()
    .find_map(|prefix| {
        leaf.strip_prefix(prefix)
            .filter(|model| model.starts_with("claude-"))
    })
    .unwrap_or(leaf)
}

/// The `(major, minor)` version written right after `stem` in `leaf`, when `leaf` starts with `stem`.
fn version_after(leaf: &str, stem: &str) -> Option<(u32, u32)> {
    let rest = leaf.strip_prefix(stem)?;
    let (major, tail) = split_digits(rest);
    let major = major.parse().ok()?;
    // A minor version is at most two digits; longer runs are dates such as `20250514`.
    let minor = tail
        .strip_prefix(['.', '-'])
        .map(split_digits)
        .filter(|(digits, _)| digits.len() <= 2)
        .and_then(|(digits, _)| digits.parse().ok())
        .unwrap_or(0);
    Some((major, minor))
}

fn split_digits(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    text.split_at(end)
}

#[cfg(test)]
mod tests;

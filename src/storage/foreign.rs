//! Directories where other coding agents keep skills and slash commands.
//!
//! Dial reads these read-only as lower-precedence sources than its own
//! `.dial` directories. Project entries are relative to the workspace root
//! (and confined there); user entries are relative to the home directory.
//! Discovery, `/skills sources` and the resource watcher all read this one
//! table so they cannot drift apart.

/// One foreign source: the provider label shown to the user and the directory
/// relative to its base (workspace root or home).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignRoot {
    pub provider: &'static str,
    pub rel: &'static str,
}

/// Skill roots inside the workspace (`<project_root>/<rel>/<name>/SKILL.md`).
pub const PROJECT_SKILL_ROOTS: &[ForeignRoot] = &[
    ForeignRoot {
        provider: "Claude",
        rel: ".claude/skills",
    },
    ForeignRoot {
        provider: "Codex",
        rel: ".codex/skills",
    },
    ForeignRoot {
        provider: "Agents",
        rel: ".agents/skills",
    },
    ForeignRoot {
        provider: "Agent",
        rel: ".agent/skills",
    },
    ForeignRoot {
        provider: "pi",
        rel: ".pi/skills",
    },
    ForeignRoot {
        provider: "Gemini",
        rel: ".gemini/skills",
    },
    ForeignRoot {
        provider: "Cursor",
        rel: ".cursor/skills",
    },
    ForeignRoot {
        provider: "Windsurf",
        rel: ".windsurf/skills",
    },
    ForeignRoot {
        provider: "Cline",
        rel: ".cline/skills",
    },
    ForeignRoot {
        provider: "GitHub",
        rel: ".github/skills",
    },
];

/// Skill roots under the home directory (`~/<rel>/<name>/SKILL.md`).
pub const USER_SKILL_ROOTS: &[ForeignRoot] = &[
    ForeignRoot {
        provider: "Claude",
        rel: ".claude/skills",
    },
    ForeignRoot {
        provider: "Codex",
        rel: ".codex/skills",
    },
    ForeignRoot {
        provider: "Agents",
        rel: ".agents/skills",
    },
    ForeignRoot {
        provider: "Agent",
        rel: ".agent/skills",
    },
    ForeignRoot {
        provider: "pi",
        rel: ".pi/agent/skills",
    },
    ForeignRoot {
        provider: "Gemini",
        rel: ".gemini/skills",
    },
    ForeignRoot {
        provider: "Cursor",
        rel: ".cursor/skills",
    },
    ForeignRoot {
        provider: "Windsurf",
        rel: ".codeium/windsurf/skills",
    },
    ForeignRoot {
        provider: "Cline",
        rel: ".cline/skills",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_have_no_duplicate_directories() {
        for table in [PROJECT_SKILL_ROOTS, USER_SKILL_ROOTS] {
            let mut rels: Vec<&str> = table.iter().map(|r| r.rel).collect();
            rels.sort_unstable();
            let before = rels.len();
            rels.dedup();
            assert_eq!(before, rels.len());
        }
    }

    #[test]
    fn entries_are_relative_and_confined() {
        for r in PROJECT_SKILL_ROOTS.iter().chain(USER_SKILL_ROOTS) {
            assert!(
                !r.rel.starts_with('/') && !r.rel.contains(".."),
                "{}",
                r.rel
            );
        }
    }
}

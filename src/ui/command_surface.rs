//! One small everyday command surface shared by slash completion and the action palette.
//! Other registered commands remain callable and searchable under Advanced.
pub const EVERYDAY: &[&str] = &[
    "model",
    "providers",
    "settings",
    "workers",
    "processes",
    "sessions",
    "resume",
    "rename",
    "fork",
    "new",
    "plan",
    "review",
    "changes",
    "undo",
    "permissions",
    "skills",
    "memory",
    "queue",
    "cost",
    "help",
    "exit",
];

pub fn everyday(name: &str) -> bool {
    EVERYDAY.contains(&name)
}

/// Where a command lives in `/help` and the action palette. The order of [`Category::ALL`] is the
/// order sections appear in; advanced categories are collapsed by default but stay searchable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Session,
    Model,
    Review,
    Work,
    Modes,
    Context,
    Integrations,
    Settings,
    Maintenance,
    Learning,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::Session,
        Category::Model,
        Category::Review,
        Category::Work,
        Category::Modes,
        Category::Context,
        Category::Integrations,
        Category::Settings,
        Category::Maintenance,
        Category::Learning,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Session => "Sessions",
            Category::Model => "Models",
            Category::Review => "Changes & review",
            Category::Work => "Tasks & agents",
            Category::Modes => "Modes",
            Category::Context => "Context & memory",
            Category::Integrations => "Integrations",
            Category::Settings => "Settings & setup",
            Category::Maintenance => "Maintenance",
            Category::Learning => "Learning",
        }
    }

    /// Short label for the palette's tab bar, where every tab must fit one row.
    pub fn tab(self) -> &'static str {
        match self {
            Category::Session => "Sessions",
            Category::Model => "Models",
            Category::Review => "Changes",
            Category::Work => "Tasks",
            Category::Modes => "Modes",
            Category::Context => "Context",
            Category::Integrations => "Tools",
            Category::Settings => "Settings",
            Category::Maintenance => "Upkeep",
            Category::Learning => "Learn",
        }
    }

    /// One line on what the section is for, shown under its heading in `/help`.
    pub fn blurb(self) -> &'static str {
        match self {
            Category::Session => "start, find, branch and share conversations",
            Category::Model => "pick models, providers and how hard they think",
            Category::Review => "see, review, commit or undo what changed",
            Category::Work => "plans, goals, background work and subagents",
            Category::Modes => "change how the agent works for a while",
            Category::Context => "what the agent knows, from memory and skills to added folders",
            Category::Integrations => "extensions, MCP servers, git, SSH and collaboration",
            Category::Settings => "appearance, keys, permissions and first-time setup",
            Category::Maintenance => "storage, updates, benchmarks and schedules",
            Category::Learning => "self-improvement, reliability and decision services",
        }
    }

    /// Collapsed in the default `/help` and the unfiltered palette; `/help all` shows them.
    pub fn advanced(self) -> bool {
        matches!(self, Category::Maintenance | Category::Learning)
    }
}

/// The category of a built-in command. Every registered command is listed; anything unknown
/// (a command added without updating this map) lands in Settings so it is still reachable.
pub fn category(name: &str) -> Category {
    match name {
        "new" | "clear" | "sessions" | "resume" | "fork" | "rename" | "export" | "tree"
        | "archive" | "copy" | "search" | "exit" | "restart" | "share" | "branch" | "delete"
        | "move" | "pin" | "session" | "dump" | "retry" => Category::Session,
        "model" | "providers" | "profile" | "reasoning" | "thinking" | "effort" | "fast"
        | "slow" | "extended-context" | "modelpreset" | "switch" | "cost" | "usage" => {
            Category::Model
        }
        "review" | "review-file" | "changes" | "undo" | "rollback" | "commit" | "checks"
        | "plan-review" | "changelog" => Category::Review,
        "workers" | "processes" | "bg" | "jobs" | "agents" | "message" | "cancel" | "queue"
        | "todo" | "goal" | "guided-goal" | "loop" | "btw" | "tan" | "compute" => Category::Work,
        "plan" | "plan-yolo" | "advisor" | "external-thinking" | "vibe" | "skillful"
        | "prewalk" | "ratchet" | "omfg" | "debug" | "fresh" | "handoff" | "shake" | "cleanse" => {
            Category::Modes
        }
        "context" | "compact" | "memory" | "skills" | "commands" | "add-dir" | "remove-dir"
        | "dirs" | "trace" => Category::Context,
        "extensions" | "mcp" | "plugins" | "marketplace" | "reload-plugins" | "tools"
        | "computer" | "ssh" | "git" | "wt" | "security" | "collab" | "join" | "leave" | "hub"
        | "relay" | "hooks" | "login" | "logout" => Category::Integrations,
        "gc" | "anonymize" | "images" | "update" | "benchmark" | "schedule" | "stats" => {
            Category::Maintenance
        }
        "improve" | "reliability" | "policy" | "steering" | "experience" | "understanding"
        | "laya" | "outcome" | "autoresearch" => Category::Learning,
        _ => Category::Settings,
    }
}

#[cfg(test)]
mod category_tests {
    use super::*;

    #[test]
    fn everyday_commands_live_in_visible_sections() {
        for name in EVERYDAY {
            assert!(
                !category(name).advanced(),
                "/{name} is everyday but its section is collapsed"
            );
        }
    }

    #[test]
    fn every_category_has_a_palette_tab() {
        let tabs = [
            "Sessions", "Models", "Changes", "Tasks", "Modes", "Context", "Tools", "Settings",
            "Upkeep", "Learn",
        ];
        for c in Category::ALL {
            assert!(tabs.contains(&c.tab()), "{:?}", c);
        }
    }

    #[test]
    fn titles_and_blurbs_are_present_and_distinct() {
        let mut titles = std::collections::HashSet::new();
        for c in Category::ALL {
            assert!(!c.title().is_empty() && !c.blurb().is_empty());
            assert!(titles.insert(c.title()), "duplicate {}", c.title());
        }
    }
}

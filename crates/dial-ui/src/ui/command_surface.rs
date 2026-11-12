//! One small everyday command surface shared by slash completion, the palette and `/help`.
//! Other registered commands remain callable and searchable under Advanced.

/// Commands offered in the everyday menu and palette. Every starter list below is a subset.
pub const EVERYDAY: &[&str] = &[
    "model",
    "providers",
    "settings",
    "workers",
    "processes",
    "sessions",
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
    "setup",
    "compact",
    "theme",
    "extensions",
];

/// Leading slash-menu entries when `/` is typed alone, in order.
pub const START_HERE: &[&str] = &[
    "help",
    "model",
    "settings",
    "providers",
    "extensions",
    "setup",
    "sessions",
    "compact",
    "review",
    "changes",
    "undo",
    "memory",
    "cost",
    "theme",
];

/// Palette entries that lead the unfiltered All list, in order.
pub const STARTERS: &[&str] = &[
    "model",
    "providers",
    "settings",
    "help",
    "setup",
    "compact",
    "theme",
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
            Category::Work => "Tasks & workers",
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

mod commands;
pub use commands::{
    ArgumentPolicy, Availability, AvailabilityContext, COMMANDS, CommandAlias, CommandSpec,
    DuringTurn, LiveForm, Surface, busy_effect, command_spec, unavailable_reason,
};

/// Category lookup is a thin projection of the shared command table.
pub fn category(name: &str) -> Category {
    command_spec(name).map_or(Category::Settings, |spec| {
        spec.aliases
            .iter()
            .find(|alias| alias.name == name)
            .map_or(spec.category, |alias| alias.category)
    })
}

#[cfg(test)]
mod category_tests;

#[cfg(test)]
mod starter_tests;

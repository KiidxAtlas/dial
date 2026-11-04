//! Neutral command metadata shared by application dispatch and UI discovery.
use super::Category;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuringTurn {
    Now,
    Stop,
    AfterTurn,
    Unavailable,
}

/// Argument-sensitive scheduling is interpreted by the application, never by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgumentPolicy {
    None,
    SideThread,
    Security,
    Setup,
    Mcp,
    CodeHistory,
    Computer,
    Model,
}

/// Contextual requirements, evaluated from the shell's cached state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Availability {
    #[default]
    Always,
    NeedsGit,
    NeedsIdle,
    NeedsSession,
}

/// No filesystem probes belong in discovery. `None` means git discovery is still pending.
#[derive(Debug, Clone, Copy, Default)]
pub struct AvailabilityContext {
    pub git: Option<bool>,
    pub busy: bool,
    pub session: bool,
}

impl Availability {
    pub fn reason(self, context: AvailabilityContext) -> Option<&'static str> {
        match self {
            Self::NeedsGit if context.git == Some(false) => Some("needs a git repository"),
            Self::NeedsIdle if context.busy => {
                Some("needs an idle session; finish or cancel the current turn")
            }
            Self::NeedsSession if !context.session => Some("needs an active session"),
            _ => None,
        }
    }
}

/// Argument-sensitive requirements share the same explanations as whole-command requirements.
pub fn unavailable_reason(
    name: &str,
    args: &str,
    context: AvailabilityContext,
) -> Option<&'static str> {
    let spec = command_spec(name)?;
    if spec.argument_policy == ArgumentPolicy::Model && args.trim() == "restart" {
        return Availability::NeedsIdle.reason(context);
    }
    spec.availability.reason(context)
}

#[derive(Debug, Clone, Copy)]
pub struct CommandAlias {
    pub name: &'static str,
    pub category: Category,
}

#[derive(Debug, Clone, Copy)]
pub struct LiveForm {
    pub args: &'static str,
    pub order: usize,
}

/// Compatibility projections retain historical RPC and completion ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Builtin,
    Group,
    Undiscovered,
}

#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub category: Category,
    pub aliases: &'static [CommandAlias],
    pub during_turn: DuringTurn,
    pub visible: bool,
    pub cli: Option<&'static str>,
    pub availability: Availability,
    pub subcommands: &'static [&'static str],
    pub argument_policy: ArgumentPolicy,
    pub live_forms: &'static [LiveForm],
    pub note: Option<&'static str>,
    pub surface: Surface,
    pub help_order: Option<usize>,
}

impl CommandSpec {
    const fn new(
        name: &'static str,
        usage: &'static str,
        description: &'static str,
        category: Category,
        surface: Surface,
    ) -> Self {
        Self {
            name,
            usage,
            description,
            category,
            surface,
            aliases: &[],
            during_turn: DuringTurn::AfterTurn,
            visible: true,
            cli: None,
            availability: Availability::Always,
            subcommands: &[],
            argument_policy: ArgumentPolicy::None,
            live_forms: &[],
            note: None,
            help_order: None,
        }
    }
}

/// Add each command exactly once here. Handlers contain execution only.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &[
            "attention",
            "workers",
            "processes",
            "downloads",
            "compute",
            "schedule",
            "queue",
            "resume",
        ],
        ..CommandSpec::new(
            "activity",
            "[attention|workers|processes|downloads|compute|schedule|queue|resume]",
            "attention, workers, processes, downloads, waits and reviewed interrupted-worker recovery",
            Category::Work,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        ..CommandSpec::new(
            "controls",
            "",
            "session controls and active versus saved settings",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["plan", "approval", "vibe", "skillful"],
        ..CommandSpec::new(
            "mode",
            "[plan [on|off]|approval [ask|write|auto|readonly]|vibe [on|off|status]|skillful [on|off|status]]",
            "choose session working modes and tool approval",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &[
            "panel", "status", "jobs", "stop", "cycle", "train", "session", "enable", "disable",
            "versions", "tasks", "rollback", "settings", "models",
        ],
        ..CommandSpec::new(
            "learn",
            "[panel|status|jobs|stop <id>|cycle|train|session|enable|disable|versions|tasks|rollback [version]|settings|models]",
            "learning and self-improvement actions",
            Category::Learning,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(0),
        ..CommandSpec::new(
            "worker",
            "<message>",
            "start independent work while a turn runs",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        aliases: &[
            CommandAlias {
                name: "jobs",
                category: Category::Work,
            },
            CommandAlias {
                name: "hub",
                category: Category::Integrations,
            },
        ],
        during_turn: DuringTurn::Now,
        note: Some("Opens now; m messages or resumes a worker, x x cancels a running one."),
        help_order: Some(21),
        ..CommandSpec::new(
            "workers",
            "[id]",
            "open the worker panel: results, cancel and message",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        aliases: &[CommandAlias {
            name: "bg",
            category: Category::Work,
        }],
        during_turn: DuringTurn::Now,
        note: Some("Opens now; keystrokes go to the attached job."),
        help_order: Some(7),
        ..CommandSpec::new(
            "processes",
            "",
            "open background processes and attach to a job's terminal",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        during_turn: DuringTurn::Now,
        note: Some("Opens now; changes apply after this turn."),
        help_order: Some(6),
        ..CommandSpec::new(
            "compute",
            "",
            "compute jobs and local runtimes",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(27),
        availability: Availability::NeedsGit,
        ..CommandSpec::new(
            "changes",
            "",
            "list changed files and line counts for this workspace",
            Category::Review,
            Surface::Builtin,
        )
    },
    CommandSpec {
        aliases: &[
            CommandAlias {
                name: "palette",
                category: Category::Settings,
            },
            CommandAlias {
                name: "?",
                category: Category::Settings,
            },
        ],
        during_turn: DuringTurn::Now,
        help_order: Some(12),
        ..CommandSpec::new(
            "help",
            "[all|text|commands]",
            "search actions and commands; text lists them",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        during_turn: DuringTurn::Now,
        help_order: Some(25),
        ..CommandSpec::new(
            "queue",
            "[edit|remove] <number>",
            "review queued follow-ups; edit or remove one",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        cli: Some("review"),
        ..CommandSpec::new(
            "review",
            "[files [turn] [path]|uncommitted|base <ref>|ref <ref>|commit <ref>|pr://N|PR URL]",
            "AI review or keep and revert checkpointed files",
            Category::Review,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        ..CommandSpec::new(
            "review-file",
            "[turn] [path]",
            "keep or revert a checkpoint's changes; AI review is /review",
            Category::Review,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "seance",
            "<session id prefix or worker id> <question>",
            "ask a read-only consultant about a recorded session",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        aliases: &[CommandAlias {
            name: "models",
            category: Category::Settings,
        }],
        during_turn: DuringTurn::Now,
        argument_policy: ArgumentPolicy::Model,
        note: Some("Opens now; changes apply after this turn. Restart is unavailable while busy."),
        help_order: Some(1),
        cli: Some("models"),
        ..CommandSpec::new(
            "model",
            "[settings|defaults|local]",
            "choose the main model and assign roles",
            Category::Model,
            Surface::Builtin,
        )
    },
    CommandSpec {
        aliases: &[CommandAlias {
            name: "provider",
            category: Category::Settings,
        }],
        during_turn: DuringTurn::Now,
        note: Some("Opens now; changes apply after this turn."),
        help_order: Some(4),
        ..CommandSpec::new(
            "providers",
            "",
            "set up providers, keys and endpoints",
            Category::Model,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        note: Some("Opens now; changes apply after this turn."),
        help_order: Some(5),
        ..CommandSpec::new(
            "settings",
            "",
            "appearance, tools and coding preferences",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        note: Some("Refreshes disk resources now; validated settings apply after this turn."),
        help_order: Some(11),
        ..CommandSpec::new(
            "reload",
            "",
            "reload settings, commands, skills and themes from disk",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(51),
        ..CommandSpec::new(
            "theme",
            "[name|auto]",
            "list or switch the color theme",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(13),
        ..CommandSpec::new(
            "hotkeys",
            "",
            "view and edit keyboard shortcuts",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        aliases: &[CommandAlias {
            name: "clear",
            category: Category::Session,
        }],
        during_turn: DuringTurn::Stop,
        help_order: Some(1),
        ..CommandSpec::new(
            "new",
            "",
            "start a fresh session",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        aliases: &[CommandAlias {
            name: "resume",
            category: Category::Session,
        }],
        during_turn: DuringTurn::Stop,
        help_order: Some(2),
        cli: Some("sessions"),
        ..CommandSpec::new(
            "sessions",
            "[id|this|branch [n]|label [text]|fork]",
            "browse all sessions or this session's branches",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Stop,
        help_order: Some(4),
        availability: Availability::NeedsSession,
        ..CommandSpec::new(
            "fork",
            "",
            "copy this conversation into a new session",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        cli: Some("sessions rename"),
        availability: Availability::NeedsSession,
        ..CommandSpec::new(
            "rename",
            "<title>",
            "name this session",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        cli: Some("sessions export"),
        availability: Availability::NeedsSession,
        ..CommandSpec::new(
            "export",
            "[path] [--html]",
            "export the conversation as Markdown or HTML",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(26),
        ..CommandSpec::new(
            "archive",
            "",
            "browse recent projects, sessions and prompts",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "autoresearch",
            "[status|init|run|log|notes|resume]",
            "manage branch-scoped benchmark experiments",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Stop,
        visible: false,
        help_order: Some(3),
        ..CommandSpec::new(
            "tree",
            "[forks]",
            "browse this session's branches; forks browses session forks",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        ..CommandSpec::new(
            "copy",
            "",
            "copy the last assistant answer",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Stop,
        help_order: Some(8),
        ..CommandSpec::new(
            "plan",
            "[on|off]",
            "toggle read-only planning mode",
            Category::Modes,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(53),
        ..CommandSpec::new(
            "profile",
            "[name|off]",
            "choose a capability profile",
            Category::Model,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "permissions",
            "[allow|deny] [--persist] <tool> [prefix] | remove <number>",
            "list, add or remove allow and deny rules",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "cost",
            "",
            "show observed runtime cost and usage coverage",
            Category::Model,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        note: Some("Opens now; m messages or resumes a worker, x x cancels a running one."),
        help_order: Some(8),
        ..CommandSpec::new(
            "agents",
            "[definitions]",
            "open the worker panel; list agent definitions",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(22),
        ..CommandSpec::new(
            "message",
            "<id> <text>",
            "steer a live subagent",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(23),
        ..CommandSpec::new(
            "cancel",
            "<id>",
            "cancel a live subagent",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Stop,
        help_order: Some(7),
        ..CommandSpec::new(
            "compact",
            "[soft|remote|snapcompact|handoff|shake] [focus]",
            "compact the conversation context",
            Category::Context,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Stop,
        help_order: Some(5),
        ..CommandSpec::new(
            "undo",
            "[n|files [n]]",
            "undo turns and files; files restores only files",
            Category::Review,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Stop,
        visible: false,
        help_order: Some(6),
        ..CommandSpec::new(
            "rollback",
            "[n]",
            "restore files from the last n turns, keeping the conversation",
            Category::Review,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(14),
        ..CommandSpec::new(
            "todo",
            "[list|show <number>|progress]",
            "inspect the plan, progress and recorded evidence",
            Category::Work,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(16),
        cli: Some("skills"),
        ..CommandSpec::new(
            "skills",
            "[list|install|update|remove|enable|disable|import|sources]",
            "browse, install and manage skills",
            Category::Context,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "memory",
            "[clear [project|global]]",
            "what Dial remembers for this project",
            Category::Context,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(19),
        ..CommandSpec::new(
            "reliability",
            "",
            "live tool success, corrections and verification",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(20),
        ..CommandSpec::new(
            "policy",
            "",
            "live task phase, effective settings and request metrics",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "steering",
            "",
            "activation vector, evaluation and runtime use",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        cli: Some("experience"),
        ..CommandSpec::new(
            "experience",
            "[ledger]",
            "learned recipes, failures and investigation",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        live_forms: &[LiveForm {
            args: "clear",
            order: 41,
        }],
        cli: Some("understanding"),
        ..CommandSpec::new(
            "understanding",
            "[architecture|build|clear|query]",
            "how the project fits together",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        argument_policy: ArgumentPolicy::CodeHistory,
        live_forms: &[
            LiveForm {
                args: "search",
                order: 39,
            },
            LiveForm {
                args: "show",
                order: 40,
            },
        ],
        ..CommandSpec::new(
            "code-history",
            "[words] [path:<p>] [branch:<b>] | show <id> | clear",
            "search diffs Dial's turns made, across sessions",
            Category::Review,
            Surface::Builtin,
        )
    },
    CommandSpec {
        live_forms: &[
            LiveForm {
                args: "on",
                order: 37,
            },
            LiveForm {
                args: "off",
                order: 38,
            },
        ],
        ..CommandSpec::new(
            "laya",
            "[status|check|start|on|off]",
            "local decision advice",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        visible: false,
        help_order: Some(17),
        ..CommandSpec::new(
            "commands",
            "",
            "list custom commands",
            Category::Context,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(15),
        ..CommandSpec::new(
            "search",
            "<query>",
            "search past sessions",
            Category::Session,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(18),
        ..CommandSpec::new(
            "context",
            "",
            "show token usage",
            Category::Context,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "approval",
            "[ask|write|auto|readonly]",
            "choose when tool calls need approval",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(54),
        ..CommandSpec::new(
            "outcome",
            "<0-100> [note]",
            "record your verdict on the last answer",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        visible: false,
        live_forms: &[
            LiveForm {
                args: "status",
                order: 0,
            },
            LiveForm {
                args: "jobs",
                order: 1,
            },
            LiveForm {
                args: "stop",
                order: 2,
            },
            LiveForm {
                args: "settings",
                order: 42,
            },
            LiveForm {
                args: "models",
                order: 43,
            },
        ],
        ..CommandSpec::new(
            "improve",
            "[status|jobs|stop|cycle|train|session|enable|disable]",
            "self-improvement status and job controls",
            Category::Learning,
            Surface::Builtin,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "accounts",
            "pin",
            "pin which ChatGPT account this session uses",
            Category::Settings,
            Surface::Builtin,
        )
    },
    CommandSpec {
        aliases: &[
            CommandAlias {
                name: "quit",
                category: Category::Settings,
            },
            CommandAlias {
                name: "q",
                category: Category::Settings,
            },
        ],
        during_turn: DuringTurn::Stop,
        help_order: Some(0),
        ..CommandSpec::new("exit", "", "quit", Category::Session, Surface::Builtin)
    },
    CommandSpec {
        subcommands: &[
            "start", "view", "join", "leave", "status", "stop", "list", "abort",
        ],
        live_forms: &[
            LiveForm {
                args: "status",
                order: 16,
            },
            LiveForm {
                args: "list",
                order: 17,
            },
            LiveForm {
                args: "stop",
                order: 18,
            },
            LiveForm {
                args: "abort",
                order: 19,
            },
        ],
        ..CommandSpec::new(
            "collab",
            "[start|view|join <link>|leave|status|stop|list|abort]",
            "choose how to host, join or manage a live room",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        ..CommandSpec::new(
            "join",
            "<link>",
            "Join a collab session from a link",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        during_turn: DuringTurn::Now,
        help_order: Some(29),
        ..CommandSpec::new(
            "leave",
            "",
            "Leave the collab session you joined",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["upload", "gist", "whole-journal", "export", "copy", "dump"],
        cli: Some("share"),
        availability: Availability::NeedsSession,
        ..CommandSpec::new(
            "share",
            "[upload [gist] [whole-journal]|gist|whole-journal|export [path] [--html]|copy|dump [all|anon]]",
            "share a link, export a transcript or copy the last answer",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        subcommands: &["all", "anon"],
        ..CommandSpec::new(
            "dump",
            "[all|anon]",
            "Write this session's transcript to a file and print its path",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "trace",
            "",
            "Open this session in the stats dashboard",
            Category::Context,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["on", "off", "status"],
        argument_policy: ArgumentPolicy::Computer,
        live_forms: &[
            LiveForm {
                args: "status",
                order: 3,
            },
            LiveForm {
                args: "on",
                order: 4,
            },
            LiveForm {
                args: "off",
                order: 5,
            },
        ],
        ..CommandSpec::new(
            "computer",
            "on|off|status",
            "control the desktop: screenshots and input",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &[
            "plan",
            "scan",
            "status",
            "cancel",
            "scans",
            "cloud",
            "show",
            "import",
            "export",
            "validate",
            "compare",
            "disposition",
        ],
        argument_policy: ArgumentPolicy::Security,
        live_forms: &[
            LiveForm {
                args: "plan",
                order: 6,
            },
            LiveForm {
                args: "status",
                order: 7,
            },
            LiveForm {
                args: "cancel",
                order: 8,
            },
            LiveForm {
                args: "scans",
                order: 9,
            },
            LiveForm {
                args: "show",
                order: 10,
            },
            LiveForm {
                args: "import",
                order: 11,
            },
            LiveForm {
                args: "export",
                order: 12,
            },
            LiveForm {
                args: "compare",
                order: 13,
            },
            LiveForm {
                args: "disposition",
                order: 14,
            },
            LiveForm {
                args: "cloud",
                order: 15,
            },
        ],
        ..CommandSpec::new(
            "security",
            "<subcommand>",
            "security review: plan, scans, import, export, compare, cloud",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &[
            "list",
            "add",
            "remove",
            "enable",
            "disable",
            "test",
            "login",
            "reload",
            "reconnect",
            "logout",
            "resources",
            "prompts",
            "prompt",
            "notifications",
            "smithery-search",
            "smithery-install",
            "smithery-login",
            "smithery-logout",
            "help",
        ],
        argument_policy: ArgumentPolicy::Mcp,
        live_forms: &[
            LiveForm {
                args: "list",
                order: 20,
            },
            LiveForm {
                args: "help",
                order: 21,
            },
            LiveForm {
                args: "add",
                order: 22,
            },
            LiveForm {
                args: "remove",
                order: 23,
            },
            LiveForm {
                args: "enable",
                order: 24,
            },
            LiveForm {
                args: "disable",
                order: 25,
            },
            LiveForm {
                args: "reload",
                order: 26,
            },
            LiveForm {
                args: "test",
                order: 27,
            },
            LiveForm {
                args: "login",
                order: 28,
            },
            LiveForm {
                args: "logout",
                order: 29,
            },
            LiveForm {
                args: "resources",
                order: 30,
            },
            LiveForm {
                args: "prompts",
                order: 31,
            },
            LiveForm {
                args: "notifications",
                order: 32,
            },
            LiveForm {
                args: "smithery-logout",
                order: 33,
            },
        ],
        ..CommandSpec::new(
            "mcp",
            "[list|add|remove|enable|disable|test|login|reload|help]",
            "manage MCP servers: list, add, remove, enable, disable, test, login, reload",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "tools",
            "",
            "list the tools offered to the agent and those available",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["mcp", "skills", "tools", "hooks"],
        help_order: Some(10),
        ..CommandSpec::new(
            "extensions",
            "[mcp|skills|tools|hooks]",
            "browse and install MCP servers and skills (/status shows the configuration)",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["tree", "html", "callflow"],
        help_order: Some(39),
        ..CommandSpec::new(
            "graphify",
            "[tree|html|callflow]",
            "open the project's Graphify knowledge graph in the browser (default: tree)",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["generate", "edit", "settings"],
        live_forms: &[LiveForm {
            args: "settings",
            order: 44,
        }],
        ..CommandSpec::new(
            "image",
            "generate <prompt> | edit <file> <prompt> | settings",
            "generate or edit an image with image_gen (asks for approval) or tune the model",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["inspect", "lineage", "compare", "correct"],
        ..CommandSpec::new(
            "evidence",
            "inspect <citation> | lineage <citation>... | compare <left> <right> | correct <citation> <text>",
            "inspect citations, lineage, revisions, gaps and append a correction",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["on", "ultra", "off", "status"],
        help_order: Some(44),
        ..CommandSpec::new(
            "fast",
            "[on|ultra|off|status]",
            "priority service tier: on, ultra (OpenAI ultrafast), off, status",
            Category::Model,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["on", "off", "status"],
        help_order: Some(45),
        ..CommandSpec::new(
            "slow",
            "[on|off|status]",
            "flex service tier for lower cost: on, off, status",
            Category::Model,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["on", "off", "status"],
        help_order: Some(46),
        ..CommandSpec::new(
            "extended-context",
            "[on|off|status]",
            "request the extended context window: on, off, status",
            Category::Model,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["list", "save", "switch", "delete"],
        help_order: Some(47),
        ..CommandSpec::new(
            "modelpreset",
            "[list|save <name>|switch <name>|delete <name>]",
            "manage model presets: list, save, switch, delete",
            Category::Model,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["show", "reset"],
        help_order: Some(30),
        ..CommandSpec::new(
            "usage",
            "[show|reset [anthropic|openai-codex] [confirm]]",
            "show provider usage limits for signed-in providers",
            Category::Model,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["list", "add", "remove", "help"],
        help_order: Some(48),
        ..CommandSpec::new(
            "ssh",
            "[list|add|remove|help]",
            "manage SSH hosts: list, add, remove, help",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["list", "add", "remove", "help"],
        help_order: Some(49),
        ..CommandSpec::new(
            "checks",
            "[list|add <name> [--ext rs,py] [--timeout secs] -- <command>|remove <name>|help]",
            "list the checks Dial runs after edits; add or remove project checks",
            Category::Review,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &[
            "presets",
            "providers",
            "memory",
            "embeddings",
            "grounding",
            "image-embeddings",
            "list",
            "updates",
            "update",
        ],
        argument_policy: ArgumentPolicy::Setup,
        live_forms: &[
            LiveForm {
                args: "list",
                order: 34,
            },
            LiveForm {
                args: "<id>",
                order: 35,
            },
            LiveForm {
                args: "embeddings",
                order: 36,
            },
        ],
        ..CommandSpec::new(
            "setup",
            "[<id> [yes]]",
            "check optional capabilities, set one up, choose memory, or sign in to providers",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        cli: Some("commit"),
        availability: Availability::NeedsGit,
        ..CommandSpec::new(
            "commit",
            "[--push] [--dry-run] [--legacy] [--no-changelog] [-c <context>] [-m <model>]",
            "generate a commit message and commit the changes",
            Category::Review,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        ..CommandSpec::new(
            "add-dir",
            "<dir>",
            "Add a directory to the workspace so file tools can read and write under it",
            Category::Context,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        ..CommandSpec::new(
            "remove-dir",
            "<dir>",
            "Remove an added directory from the workspace",
            Category::Context,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["add", "remove", "list"],
        ..CommandSpec::new(
            "dirs",
            "[add <dir>|remove <dir>|list]",
            "manage workspace directories",
            Category::Context,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["create", "list", "switch", "remove"],
        ..CommandSpec::new(
            "wt",
            "[list|create [branch]|switch <path|branch>|remove <path|branch> [--force]]",
            "manage git worktrees for this session",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &[
            "tab status",
            "tab diff",
            "tab commit",
            "tab log",
            "commit run",
        ],
        ..CommandSpec::new(
            "git",
            "[tab status|diff|commit|log | commit run [flags] | revision]",
            "inspect, stage and commit changes in one git panel",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["list", "send", "claim", "clear"],
        ..CommandSpec::new(
            "peers",
            "[list|send <id> <text>|claim <paths...>|clear]",
            "show same-checkout sessions, claims and overlaps; message peers or claim paths",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        ..CommandSpec::new(
            "branch",
            "[n]",
            "Pick a user message to rewind to, after confirming (keeps the old path as a branch; alias: rewind)",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "label",
            "[text]",
            "Label the latest message so it is easy to find in /tree (no text clears the label)",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "delete",
            "",
            "Delete the current session after confirming, and start a new one",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "pin",
            "",
            "Pin or unpin a session at the top of the resume list",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "move",
            "<dir>",
            "Move the current session to a different directory",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        cli: Some("sessions pack"),
        availability: Availability::NeedsSession,
        ..CommandSpec::new(
            "pack",
            "[path]",
            "Write this session as a portable archive with its side history, workers and artifacts",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "restart",
            "",
            "Restart dial with the same launch flags, resuming this session",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(24),
        ..CommandSpec::new(
            "retry",
            "",
            "Skip a live provider retry wait; when idle, retry the last failed turn",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["--port"],
        help_order: Some(42),
        cli: Some("stats"),
        ..CommandSpec::new(
            "stats",
            "[range] [--port <port>]",
            "Show local usage statistics; --port starts the loopback dashboard",
            Category::Maintenance,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(40),
        ..CommandSpec::new(
            "login",
            "[provider]",
            "Sign in to an OAuth provider (no argument lists providers)",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(41),
        ..CommandSpec::new(
            "logout",
            "[provider]",
            "Sign out of an OAuth provider (no argument lists signed-in providers)",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["info", "delete", "pin"],
        ..CommandSpec::new(
            "session",
            "[info|delete|pin]",
            "Show the current session, or manage it (info, delete, pin)",
            Category::Session,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["full", "last"],
        help_order: Some(43),
        ..CommandSpec::new(
            "changelog",
            "[full|last [N]]",
            "Show the Dial changelog: `full`, or `last [N]` (default 1)",
            Category::Review,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["on", "off", "status"],
        note: Some(
            "Runs now; saves changes for after the current turn. Drafts still require review.",
        ),
        help_order: Some(52),
        ..CommandSpec::new(
            "drafter",
            "[on|off|status]",
            "Toggle turn drafting and review before sending to the agent",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        aliases: &[
            CommandAlias {
                name: "reasoning",
                category: Category::Model,
            },
            CommandAlias {
                name: "thinking",
                category: Category::Model,
            },
        ],
        during_turn: DuringTurn::Now,
        help_order: Some(50),
        ..CommandSpec::new(
            "effort",
            "[level|auto] | <role> <level|default>",
            "Show or set the thinking effort for the current model or a model role",
            Category::Model,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "loop",
            "[count|duration] [--while|--until '<command>'] [prompt]",
            "Repeat a prompt after each turn, bounded by a count or duration",
            Category::Work,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        subcommands: &["on", "off", "status"],
        ..CommandSpec::new(
            "vibe",
            "[on|off|status]",
            "Toggle read-only vibe mode for this session",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        subcommands: &["on", "off", "status"],
        ..CommandSpec::new(
            "skillful",
            "[on|off|status]",
            "Show or set whether the skill catalog is offered this session",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "fresh",
            "",
            "Reset provider stream state without changing the local transcript",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "handoff",
            "[focus]",
            "Summarize the session into a handoff document and compact in place",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["elide", "images", "thinking"],
        ..CommandSpec::new(
            "shake",
            "[elide|images|thinking]",
            "Drop heavy content from context (tool results, large blocks)",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        argument_policy: ArgumentPolicy::SideThread,
        ..CommandSpec::new(
            "btw",
            "[question]",
            "Browse side threads or ask a side question",
            Category::Work,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "tan",
            "<work>",
            "Fork the session into a background clone that runs one request",
            Category::Work,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &[
            "open-artifacts",
            "performance",
            "work",
            "dump",
            "memory",
            "logs",
            "system",
            "terminal",
            "protocols",
            "raw-sse",
            "remote-debugger",
            "transcript",
            "clear-cache",
        ],
        ..CommandSpec::new(
            "debug",
            "[item]",
            "Show debug tools, session/model/usage facts, and run one debug item",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "rule",
            "<complaint>",
            "Author a stream rule from a complaint and save it",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["restart", "off"],
        ..CommandSpec::new(
            "prewalk",
            "[restart|off]",
            "Arm, restart, or cancel the todo-gated handoff to @small",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "switch",
            "[model]",
            "Switch the model for this session only",
            Category::Model,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "plan-review",
            "",
            "Re-open the submitted plan for approval (plan mode only)",
            Category::Review,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["init", "plan", "split", "approve", "status", "climb"],
        ..CommandSpec::new(
            "ratchet",
            "<flow> <init|plan|split|approve|status|climb> [flags]",
            "Eval gate and hill-climb runner: init, plan, split, approve, status, climb",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["set", "show", "pause", "resume", "drop", "budget"],
        ..CommandSpec::new(
            "goal",
            "[set <objective>|show|pause|resume|drop|budget <tokens|off>]",
            "Persistent session objective: set, show, pause, resume, drop, budget",
            Category::Work,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "guided-goal",
            "<idea>",
            "Have the agent interview you in chat, then set up goal mode",
            Category::Work,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "contract",
            "<task>",
            "Start a task as a contract: goal, constraints and checkable done-when deliverables",
            Category::Work,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "cleanse",
            "[request] [--all] [--tests] [-n N] [-m model]",
            "Run project checkers and repair their diagnostics with worker subagents",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(38),
        cli: Some("watch"),
        availability: Availability::NeedsSession,
        ..CommandSpec::new(
            "watch",
            "",
            "open live edits, errors and screenshots in a separate terminal pane",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["on", "completion", "off", "status"],
        ..CommandSpec::new(
            "advisor",
            "on|completion|off|status",
            "coach reviews each tool round and the final reply (on, completion, off, status)",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["on", "off", "status"],
        ..CommandSpec::new(
            "external-thinking",
            "on|off|status",
            "private scratchpad reasoning hidden from the transcript (on, off, status)",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &["off", "status"],
        ..CommandSpec::new(
            "plan-yolo",
            "[MODEL|off|status]",
            "plan mode whose next plan is auto-approved and handed to MODEL (default @small)",
            Category::Modes,
            Surface::Group,
        )
    },
    CommandSpec {
        visible: false,
        during_turn: DuringTurn::Now,
        subcommands: &["list", "add", "remove", "log", "run"],
        help_order: Some(32),
        cli: Some("schedule"),
        ..CommandSpec::new(
            "schedule",
            "list|add|remove|log|run",
            "recurring or file-triggered prompt runs for this project",
            Category::Maintenance,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["apply"],
        help_order: Some(33),
        cli: Some("gc"),
        ..CommandSpec::new(
            "gc",
            "[apply] [--blobs|--archive|--wal|--stale]",
            "sweep stale blobs, archives and WAL state (dry run unless `apply`)",
            Category::Maintenance,
            Surface::Group,
        )
    },
    CommandSpec {
        cli: Some("anonymize"),
        ..CommandSpec::new(
            "anonymize",
            "[SESSION] [-o PATH]",
            "write an anonymized copy of a session (default: this one)",
            Category::Maintenance,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["status", "doctor", "probe", "purge"],
        help_order: Some(34),
        cli: Some("images"),
        ..CommandSpec::new(
            "images",
            "[status|doctor|probe|purge]",
            "image cache: status, doctor, probe, purge [--apply] [--all]",
            Category::Maintenance,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        help_order: Some(35),
        cli: Some("update"),
        ..CommandSpec::new(
            "update",
            "[VERSION]",
            "verify and install the latest signed release (then /restart)",
            Category::Maintenance,
            Surface::Group,
        )
    },
    CommandSpec {
        cli: Some("benchmark"),
        ..CommandSpec::new(
            "benchmark",
            "[flags]",
            "run a coding benchmark suite in the background (same flags as `dial benchmark`)",
            Category::Maintenance,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        subcommands: &["start", "stop", "status"],
        help_order: Some(36),
        cli: Some("relay"),
        ..CommandSpec::new(
            "relay",
            "start [ADDR]|stop|status",
            "local collaboration relay for /collab (start [ADDR], stop, status)",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        visible: false,
        help_order: Some(37),
        ..CommandSpec::new(
            "hooks",
            "",
            "list configured lifecycle hooks",
            Category::Integrations,
            Surface::Group,
        )
    },
    CommandSpec {
        subcommands: &[
            "status",
            "open",
            "map",
            "analyze",
            "stats",
            "activations",
            "trace",
            "experiment",
            "generate",
            "direction",
            "edit",
            "evaluate",
            "combine",
            "history",
            "show",
            "compare",
            "prune",
            "cases",
            "next",
            "improve",
            "promote",
            "revert",
        ],
        live_forms: &[
            LiveForm {
                args: "",
                order: 45,
            },
            LiveForm {
                args: "status",
                order: 46,
            },
            LiveForm {
                args: "open",
                order: 47,
            },
        ],
        cli: Some("weights"),
        ..CommandSpec::new(
            "weights",
            "[status|open|next|map|analyze|stats|activations|trace|experiment NAME [--budget N]|generate|direction NAME|edit NAME|combine NAME --edits a,b|evaluate EDIT --cases FILE|history|show ID|compare A B|prune|cases --session ID|improve [--with MODEL] GOAL|promote EDIT|revert]",
            "inspect local MLX model weights and run evaluation-gated weight edit experiments",
            Category::Learning,
            Surface::Group,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        visible: false,
        note: Some("Opens now; changes apply after this turn."),
        help_order: Some(2),
        ..CommandSpec::new("roles", "", "", Category::Settings, Surface::Undiscovered)
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        visible: false,
        note: Some("Opens now; changes apply after this turn."),
        help_order: Some(3),
        ..CommandSpec::new(
            "model-settings",
            "",
            "",
            Category::Settings,
            Surface::Undiscovered,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        visible: false,
        help_order: Some(28),
        ..CommandSpec::new("status", "", "", Category::Settings, Surface::Undiscovered)
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        visible: false,
        help_order: Some(31),
        ..CommandSpec::new(
            "marketplace",
            "",
            "",
            Category::Settings,
            Surface::Undiscovered,
        )
    },
    CommandSpec {
        during_turn: DuringTurn::Now,
        note: Some("Opens now; changes apply after this turn."),
        cli: Some("config"),
        ..CommandSpec::new(
            "set",
            "<key> [value] [--project]",
            "show a setting and its source, or save a validated value",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        ..CommandSpec::new(
            "inbox",
            "[id]",
            "read session notifications",
            Category::Settings,
            Surface::Group,
        )
    },
    CommandSpec {
        cli: Some("doctor"),
        ..CommandSpec::new(
            "doctor",
            "[id [yes]]",
            "diagnose setup, auth and model problems and offer fixes",
            Category::Settings,
            Surface::Group,
        )
    },
];

pub fn command_spec(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.iter().any(|alias| alias.name == name))
}

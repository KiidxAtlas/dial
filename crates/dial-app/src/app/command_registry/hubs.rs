//! Navigation adapters only: execution remains in the original command handlers.
use crate::ui::overlay::palette::{ChoiceItem, ChoiceSpec};

/// Expand hub actions without rewriting legacy command names or their arguments.
pub(in crate::app) fn expand<'a>(name: &'a str, args: &'a str) -> (&'a str, &'a str) {
    let args = args.trim();
    let (sub, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(sub, rest)| (sub, rest.trim()));
    match (name, sub) {
        ("mode", "plan" | "approval" | "vibe" | "skillful") => (sub, rest),
        ("dirs", "add") => ("add-dir", rest),
        ("dirs", "remove") => ("remove-dir", rest),
        ("collab", "join") => ("join", rest),
        ("collab", "leave") => ("leave", rest),
        ("share", "export" | "copy" | "dump") => (sub, rest),
        ("learn", "panel") if rest.is_empty() => ("improve", ""),
        ("learn", sub) if !sub.is_empty() => ("improve", args),
        _ => (name, args),
    }
}

/// Actions needing free text return to the composer rather than running an empty operation.
pub(in crate::app) fn draft(name: &str, args: &str) -> Option<&'static str> {
    match (name, args.trim()) {
        ("dirs", "add") => Some("/dirs add "),
        ("dirs", "remove") => Some("/dirs remove "),
        ("collab", "join") => Some("/collab join "),
        _ => None,
    }
}

pub(in crate::app) fn picker(name: &str, args: &str) -> Option<ChoiceSpec> {
    if !args.trim().is_empty() {
        return None;
    }
    let (title, rows): (&str, &[(&str, &str, &str)]) = match name {
        "mode" => (
            "Working mode",
            &[
                (
                    "plan on",
                    "Plan · on",
                    "Read-only planning; does not change approval policy.",
                ),
                ("plan off", "Plan · off", "Return to normal execution."),
                (
                    "approval",
                    "Tool approval policy",
                    "Choose ask, write, auto or readonly.",
                ),
                (
                    "vibe on",
                    "Read-only tools · on",
                    "Offer only read-only tools for this session.",
                ),
                (
                    "vibe off",
                    "Read-only tools · off",
                    "Restore the normal tool set.",
                ),
                (
                    "skillful on",
                    "Skill catalog · on",
                    "Offer skills for this session.",
                ),
                (
                    "skillful off",
                    "Skill catalog · off",
                    "Hide skills for this session.",
                ),
            ],
        ),
        "dirs" => (
            "Workspace directories",
            &[
                (
                    "list",
                    "List directories",
                    "Show the primary root and added roots.",
                ),
                ("add", "Add a directory…", "Enter a path in the composer."),
                (
                    "remove",
                    "Remove a directory…",
                    "Enter an added root in the composer.",
                ),
            ],
        ),
        "collab" => (
            "Live collaboration",
            &[
                (
                    "start",
                    "Host a session",
                    "Anyone with the link can read and prompt the agent.",
                ),
                (
                    "view",
                    "Host read-only",
                    "Anyone with the link can read, but cannot prompt.",
                ),
                (
                    "join",
                    "Join from a link…",
                    "Enter a collaboration link in the composer.",
                ),
                (
                    "status",
                    "Room status",
                    "Show the current host or guest connection.",
                ),
                (
                    "list",
                    "Local hosts",
                    "List live hosts on this machine; never reveal links.",
                ),
                (
                    "leave",
                    "Leave as guest",
                    "Disconnect from the room you joined.",
                ),
                ("stop", "Stop hosting", "Close your hosted room."),
                (
                    "abort",
                    "Stop the host's turn",
                    "Request cancellation as a guest.",
                ),
            ],
        ),
        "learn" => (
            "Learning and improvement",
            &[
                (
                    "panel",
                    "Improvement panel",
                    "Review the existing learning dashboard.",
                ),
                ("status", "Status", "Show self-improvement status."),
                ("jobs", "Jobs", "Review improvement jobs."),
                (
                    "settings",
                    "Learning settings",
                    "Configure the learning loop.",
                ),
                (
                    "models",
                    "Learning models",
                    "Choose models used by learning.",
                ),
                (
                    "cycle",
                    "Run a cycle",
                    "Run the existing improvement cycle.",
                ),
                ("train", "Train", "Start the existing training action."),
                (
                    "session",
                    "Learn from this session",
                    "Run the existing session learning action.",
                ),
                (
                    "enable",
                    "Enable automatic improvement",
                    "Resume automatic improvement.",
                ),
                (
                    "disable",
                    "Pause automatic improvement",
                    "Disable automatic improvement.",
                ),
            ],
        ),
        "share" => (
            "Share and export",
            &[
                (
                    "upload",
                    "Encrypted share link",
                    "Upload the active branch; anyone with the full link can read it.",
                ),
                (
                    "gist",
                    "Encrypted gist link",
                    "Upload through gh as a secret gist.",
                ),
                (
                    "export",
                    "Export Markdown",
                    "Write the conversation to the existing default path.",
                ),
                (
                    "export --html",
                    "Export HTML",
                    "Write the conversation as HTML.",
                ),
                (
                    "dump",
                    "Temporary transcript file",
                    "Write a raw transcript; it may contain secrets.",
                ),
                (
                    "copy",
                    "Copy last answer",
                    "Copy only the last assistant answer, not the transcript.",
                ),
            ],
        ),
        _ => return None,
    };
    Some(ChoiceSpec {
        title: title.into(),
        command: name.into(),
        items: rows
            .iter()
            .map(|(value, label, detail)| ChoiceItem {
                value: (*value).into(),
                label: (*label).into(),
                detail: (*detail).into(),
                current: false,
            })
            .collect(),
        summary: "Choose an action; Esc returns without changing anything.".into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests;

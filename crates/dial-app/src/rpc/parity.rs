//! Where each slash command lives for an RPC client. Every declared command is classified here,
//! and a test fails when a new command is added without a classification, so a GUI always
//! knows whether to call a method, render the feature itself, or tell the user it is terminal-only.
//! `get_available_commands` reports the classification next to each command as `rpc`.
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Parity {
    /// The command's engine operation is this RPC method.
    Method(&'static str),
    /// Presentation the client owns (panels, themes, keys, clipboard, its own process).
    Client(&'static str),
    /// Only the terminal serves it today; the reason says why or what to use instead.
    Terminal(&'static str),
}

impl Parity {
    pub(super) fn json(self) -> Value {
        match self {
            Self::Method(method) => json!({"surface":"method","method":method}),
            Self::Client(reason) => json!({"surface":"client","reason":reason}),
            Self::Terminal(reason) => json!({"surface":"terminal","reason":reason}),
        }
    }
}

const PRESENTATION: &str = "presentation the client renders itself";
const PANEL: &str = "terminal panel or workflow; not an RPC method yet";
const GIT: &str = "git workflows run in the terminal or through `dial commit` / `git`";
const LEARNING: &str = "learning and evaluation tooling runs from the terminal or the `dial` CLI";
const COLLAB: &str = "live collaboration rooms are a terminal feature";
const MODES: &str = "session working mode toggled in the terminal; plan mode is the `plan` method";

/// The RPC surface of the canonical command `name`; `None` for a command not classified yet.
pub(super) fn parity(name: &str) -> Option<Parity> {
    use Parity::{Client, Method, Terminal};
    Some(match name {
        "worker" => Method("task_start"),
        "workers" | "agents" => Method("task_status"),
        "message" => Method("task_message"),
        "cancel" => Method("task_cancel"),
        "processes" => Method("list_jobs"),
        "review-file" => Method("checkpoint_diff"),
        "rollback" => Method("restore_checkpoint_files"),
        "undo" => Method("undo"),
        "model" | "switch" => Method("set_model"),
        "effort" => Method("set_thinking"),
        "settings" => Method("get_settings"),
        "set" | "roles" | "model-settings" => Method("set_setting"),
        "reload" => Method("apply_settings"),
        "new" => Method("new_session"),
        "sessions" => Method("list_sessions"),
        "fork" => Method("fork_session"),
        "rename" => Method("rename"),
        "export" => Method("export"),
        "plan" => Method("plan"),
        "cost" | "context" => Method("get_session_stats"),
        "tools" | "status" | "session" => Method("state"),
        "compact" => Method("compact"),
        "memory" => Method("memory"),
        "commands" => Method("get_available_commands"),
        "approval" => Method("set_approval"),
        "add-dir" => Method("add_dir"),
        "remove-dir" => Method("remove_dir"),
        "dirs" => Method("list_dirs"),
        "delete" => Method("delete_session"),
        "pin" => Method("pin_session"),
        "retry" => Method("retry"),
        "login" => Method("login"),
        "logout" => Method("logout"),
        "extensions" => Method("extension_action"),
        "mcp" => Method("mcp_login"),
        "exit" => Method("shutdown"),
        "activity" | "controls" | "help" | "theme" | "hotkeys" | "copy" | "queue" | "draft"
        | "attachments" | "watch" => Client(PRESENTATION),
        "restart" => Client("restart the `dial rpc` process; `resume` reopens the session"),
        "computer" => Terminal(
            "enable it with set_setting `integrations` and apply_settings; OS permission checks are terminal-only",
        ),
        "doctor" | "setup" => Terminal("interactive repair and setup flows; use `dial doctor`"),
        "update" => Terminal("installs a new binary; use `dial update`"),
        "providers" => Terminal("provider setup wizard; use set_setting and login"),
        "commit" | "git" | "wt" | "changes" | "code-history" => Terminal(GIT),
        "learn" | "improve" | "weights" | "steering" | "experience" | "autoresearch"
        | "benchmark" | "ratchet" | "outcome" | "laya" => Terminal(LEARNING),
        "collab" | "join" | "leave" | "relay" | "peers" => Terminal(COLLAB),
        "mode" | "vibe" | "skillful" | "drafter" | "advisor" | "external-thinking"
        | "plan-yolo" | "fast" | "slow" | "extended-context" | "prewalk" => Terminal(MODES),
        "compute" | "review" | "seance" | "archive" | "tree" | "profile" | "permissions"
        | "todo" | "skills" | "reliability" | "policy" | "understanding" | "search"
        | "accounts" | "share" | "dump" | "trace" | "security" | "graphify" | "image"
        | "evidence" | "modelpreset" | "usage" | "ssh" | "checks" | "branch" | "label" | "move"
        | "pack" | "stats" | "changelog" | "loop" | "fresh" | "handoff" | "shake" | "btw"
        | "tan" | "debug" | "rule" | "plan-review" | "goal" | "guided-goal" | "contract"
        | "cleanse" | "schedule" | "gc" | "anonymize" | "images" | "hooks" | "marketplace"
        | "inbox" => Terminal(PANEL),
        _ => return None,
    })
}

#[cfg(test)]
mod tests;

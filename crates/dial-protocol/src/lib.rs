//! Wire vocabulary of the plain `dial rpc` control protocol.
//!
//! The engine (`dial-app`) serves exactly these methods and codes, and clients (`dial-client`, or a
//! GUI that links this crate directly) branch on them. Keeping one table means a method added to
//! the engine automatically appears in `ready.methods`, in the client's retry rules and in the
//! conformance tests against `docs/rpc-schema.json`. The crate is plain data with no dependencies.

#[cfg(test)]
mod tests;

/// Protocol version of connections that never send `hello`.
pub const DEFAULT_PROTOCOL_VERSION: u32 = 1;
/// Latest protocol version this vocabulary describes.
pub const PROTOCOL_VERSION: u32 = 2;
/// Every protocol version served, oldest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[u32] = &[DEFAULT_PROTOCOL_VERSION, PROTOCOL_VERSION];

/// What repeating a request does when its response was lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Only reads state; repeating it on a new connection is safe.
    ReadOnly,
    /// May change the session, files or settings; its outcome is unknown after a lost response.
    Mutating,
}

/// One request method of the plain RPC endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Method {
    pub name: &'static str,
    /// Capability announced in `ready.capabilities` when the method is served.
    pub capability: &'static str,
    pub effect: Effect,
}

const fn read(name: &'static str, capability: &'static str) -> Method {
    Method {
        name,
        capability,
        effect: Effect::ReadOnly,
    }
}

const fn write(name: &'static str, capability: &'static str) -> Method {
    Method {
        name,
        capability,
        effect: Effect::Mutating,
    }
}

/// Every request method in announcement order.
pub const METHODS: &[Method] = &[
    read("hello", "query"),
    write("send", "prompt"),
    read("state", "query"),
    read("messages", "query"),
    read("messages_page", "query"),
    write("plan", "plan_mode"),
    write("set_model", "model"),
    write("set_thinking", "model"),
    read("get_session_stats", "query"),
    read("get_available_commands", "query"),
    write("export", "export"),
    write("set_host_tools", "host_tools"),
    write("host_tool_result", "host_tools"),
    write("set_host_uri_schemes", "host_uris"),
    write("host_uri_result", "host_uris"),
    read("auth_providers", "auth"),
    write("login", "auth"),
    write("auth_code", "auth"),
    write("auth_cancel", "auth"),
    write("logout", "auth"),
    write("new_session", "sessions"),
    write("resume", "sessions"),
    write("rename", "sessions"),
    write("compact", "maintenance"),
    write("memory", "maintenance"),
    write("seed_context", "context_seed"),
    write("task_start", "tasks"),
    read("task_status", "tasks"),
    write("task_cancel", "tasks"),
    write("task_message", "tasks"),
    write("steer", "steering"),
    write("abort", "abort"),
    write("approve", "approvals"),
    write("question_presented", "questions"),
    write("question_answer", "questions"),
    write("live_start", "live_voice"),
    write("live_mute", "live_voice"),
    write("live_stop", "live_voice"),
    write("live_listen", "live_voice"),
    write("shutdown", "shutdown"),
    read("list_sessions", "sessions"),
    write("fork_session", "sessions"),
    write("delete_session", "session_admin"),
    write("pin_session", "session_admin"),
    read("list_models", "model"),
    read("settings_schema", "settings"),
    read("get_settings", "settings"),
    write("set_setting", "settings"),
    write("reset_setting", "settings"),
    read("list_checkpoints", "checkpoints"),
    read("checkpoint_files", "checkpoints"),
    read("checkpoint_diff", "checkpoints"),
    write("restore_checkpoint_files", "checkpoints"),
    write("undo", "checkpoints"),
    read("list_extensions", "extensions"),
];

/// The method named `name`, if the protocol defines it.
pub fn method(name: &str) -> Option<&'static Method> {
    METHODS.iter().find(|method| method.name == name)
}

/// Whether repeating `name` after a lost response is safe. Unknown methods are not.
pub fn is_read_only(name: &str) -> bool {
    method(name).is_some_and(|method| method.effect == Effect::ReadOnly)
}

/// Stable machine code of a failed request (protocol 2 `error.code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Another operation owns the session, or a queue is full. Retry after it settles.
    Busy,
    /// The request is malformed or a value is out of range. Retrying unchanged fails again.
    InvalidParams,
    /// The targeted turn, approval or question is not (or no longer) pending.
    NotPending,
    /// The named session, model or other target does not exist.
    NotFound,
    /// A session-change hook or policy vetoed the request.
    Refused,
    /// The target changed since the client read it (a stale review token), or the change overlaps
    /// other work and needs explicit confirmation. Re-read, then retry deliberately.
    Conflict,
    /// The endpoint does not serve this method.
    UnknownMethod,
    /// `hello` asked only for versions this build does not serve.
    UnsupportedProtocolVersion,
    /// The engine hit an internal fault (for example the actor panicked).
    InternalError,
    /// Any other failure; read the message.
    Failed,
}

impl ErrorCode {
    /// Every code, in documentation order.
    pub const ALL: &'static [ErrorCode] = &[
        Self::Busy,
        Self::InvalidParams,
        Self::NotPending,
        Self::NotFound,
        Self::Refused,
        Self::Conflict,
        Self::UnknownMethod,
        Self::UnsupportedProtocolVersion,
        Self::InternalError,
        Self::Failed,
    ];

    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::InvalidParams => "invalid_params",
            Self::NotPending => "not_pending",
            Self::NotFound => "not_found",
            Self::Refused => "refused",
            Self::Conflict => "conflict",
            Self::UnknownMethod => "unknown_method",
            Self::UnsupportedProtocolVersion => "unsupported_protocol_version",
            Self::InternalError => "internal_error",
            Self::Failed => "failed",
        }
    }

    /// Parses a wire code. A code from a newer engine maps to [`ErrorCode::Failed`], so a client
    /// keeps working and falls back to the message.
    pub fn parse(code: &str) -> Self {
        Self::ALL
            .iter()
            .copied()
            .find(|known| known.as_str() == code)
            .unwrap_or(Self::Failed)
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Fields the engine adds to every plain-RPC event: `seq` (per-connection, gap-free, in stdout
/// order), `session_id` (session being served) and `request_id` (request whose processing produced
/// the event, or null for background work).
pub const ENVELOPE_FIELDS: &[&str] = &["seq", "session_id", "request_id"];

/// Every event name the plain RPC endpoint emits. Clients should ignore names they do not know.
pub const EVENTS: &[&str] = &[
    "ready",
    "text",
    "reasoning",
    "tool_call",
    "tool_card",
    "tool_arguments",
    "tool_output",
    "tool_result",
    "tool_timing",
    "policy_explanation",
    "approval",
    "approval_resolved",
    "question",
    "question_closed",
    "notice",
    "background",
    "panel",
    "todo",
    "usage",
    "cached_usage",
    "context_usage",
    "efficiency",
    "model_progress",
    "state_node",
    "state_delta",
    "state_reset",
    "active_session",
    "replay_message",
    "live_error",
    "host_tool_call",
    "host_uri_request",
    "auth_url",
    "auth_progress",
    "auth_prompt",
    "fatal",
];

//! Explicit opt-in startup preparation. These controls never bypass approvals.
use super::*;
pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "preparation.local_prefix",
        preparation.local_prefix,
        EX,
        "Preparation",
        "Local prefix warming",
        "Before the first turn, send one bounded request through the resident local backend while idle. No hosted fallback or model downloads. Output is discarded; a cache hit is not guaranteed.",
        Kind::Bool,
    ),
    field!(
        "preparation.lsp_paths",
        preparation.lsp_paths,
        EX,
        "Preparation",
        "Language server source paths",
        "Up to eight workspace-relative source files to prepare with ordinary approved LSP document-symbol requests while idle. Empty disables startup.",
        Kind::StringList,
    ),
    field!(
        "preparation.cargo_check",
        preparation.cargo_check,
        EX,
        "Preparation",
        "Prepare Rust build",
        "While idle, request an approved cargo check --locked --offline --workspace. Uses the session Cargo target and never downloads missing dependencies.",
        Kind::Bool,
    ),
    field!(
        "preparation.timeout_secs",
        preparation.timeout_secs,
        EX,
        "Preparation",
        "Preparation deadline",
        "Maximum seconds per startup preparation action. New foreground input cancels preparation.",
        Kind::Int {
            min: 1,
            max: 300,
            step: 1
        },
    ),
];

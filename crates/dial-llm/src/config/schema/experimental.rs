//! Experimental setting declarations. Defaults derive from typed configuration fields.
use super::*;
pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "experimental.work_packet_contracts",
        experimental.work_packet_contracts,
        EX,
        "Agent coordination",
        "Work packet contracts",
        "Give each subagent an explicit scope and handoff contract; require an evidence-backed interface report.",
        Kind::Bool,
    ),
    field!(
        "experimental.verification_receipts",
        experimental.verification_receipts,
        EX,
        "Verification",
        "Verification receipts",
        "Persist bounded command/status/source-digest receipts with verification results.",
        Kind::Bool,
    ),
    field!(
        "experimental.context_overlap_dedup",
        experimental.context_overlap_dedup,
        EX,
        "Context",
        "Overlap deduplication",
        "Remove exact repeated paragraphs across ambient instruction files while preserving distinct paragraphs and source labels.",
        Kind::Bool,
    ),
    field!(
        "experimental.profile_matching",
        experimental.profile_matching,
        EX,
        "Agent routing",
        "Profile matching",
        "Allow a task to select an explicitly configured model profile for its subagent; profile names must be operator-configured.",
        Kind::Bool,
    ),
    field!(
        "experimental.tool_speculation",
        experimental.tool_speculation,
        EX,
        "Execution",
        "Speculative reads",
        "Prefetch admitted native AST searches of one bounded local file. Cache reuse requires identical owner policy and source bytes. Disabled until measured on your workload.",
        Kind::Bool,
    ),
    field!(
        "experimental.turn_optimizer",
        experimental.turn_optimizer,
        EX,
        "Agent routing",
        "Turn optimizer",
        "Rewrite user and steering messages with turn_drafter. Interactive Dial shows each draft for acceptance before sending; /drafter toggles this feature. A failed or cancelled draft sends nothing.",
        Kind::Bool
    ),
];

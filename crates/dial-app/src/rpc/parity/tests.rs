use super::*;

#[test]
fn every_declared_command_has_an_rpc_surface() {
    let missing: Vec<&str> = crate::app::command_names()
        .filter(|name| parity(name).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "classify these commands in rpc/parity.rs: {missing:?}"
    );
}

#[test]
fn every_named_method_is_served_by_plain_rpc() {
    for name in crate::app::command_names() {
        if let Some(Parity::Method(method)) = parity(name) {
            let served = dial_protocol::method(method)
                .unwrap_or_else(|| panic!("/{name} names unknown method {method}"));
            assert!(
                super::super::capability_available(served.capability, false),
                "/{name} names {method}, which plain RPC does not serve"
            );
        }
    }
}

#[test]
fn surfaces_serialize_with_their_method_or_reason() {
    assert_eq!(
        parity("approval").unwrap().json(),
        json!({"surface":"method","method":"set_approval"})
    );
    assert_eq!(parity("theme").unwrap().json()["surface"], "client");
    assert_eq!(parity("commit").unwrap().json()["surface"], "terminal");
    assert!(parity("not-a-command").is_none());
}

/// Commands only the terminal serves, reviewed one by one. A new command must not be added here
/// to get a green run: give it an RPC method (or a `Client` classification when it is pure
/// presentation). Remove names as they gain methods; the test fails until the list shrinks too.
const TERMINAL_BASELINE: &[&str] = &[
    "advisor",
    "anonymize",
    "archive",
    "autoresearch",
    "branch",
    "btw",
    "changelog",
    "changes",
    "checks",
    "cleanse",
    "code-history",
    "collab",
    "commit",
    "compute",
    "computer",
    "contract",
    "debug",
    "doctor",
    "drafter",
    "dump",
    "evidence",
    "experience",
    "extended-context",
    "fast",
    "fresh",
    "gc",
    "git",
    "goal",
    "graphify",
    "guided-goal",
    "handoff",
    "hooks",
    "image",
    "images",
    "improve",
    "inbox",
    "external-thinking",
    "join",
    "label",
    "laya",
    "learn",
    "leave",
    "loop",
    "marketplace",
    "mode",
    "modelpreset",
    "move",
    "outcome",
    "pack",
    "peers",
    "permissions",
    "plan-review",
    "plan-yolo",
    "policy",
    "prewalk",
    "profile",
    "providers",
    "ratchet",
    "relay",
    "reliability",
    "review",
    "rule",
    "schedule",
    "seance",
    "search",
    "security",
    "setup",
    "shake",
    "share",
    "skillful",
    "skills",
    "slow",
    "ssh",
    "stats",
    "steering",
    "tan",
    "todo",
    "trace",
    "tree",
    "understanding",
    "update",
    "usage",
    "vibe",
    "weights",
    "wt",
    "accounts",
];

#[test]
fn terminal_only_commands_never_grow_past_the_reviewed_baseline() {
    let baseline: std::collections::BTreeSet<&str> = TERMINAL_BASELINE.iter().copied().collect();
    let terminal: std::collections::BTreeSet<&str> = crate::app::command_names()
        .filter(|name| matches!(parity(name), Some(Parity::Terminal(_))))
        .collect();
    let added: Vec<_> = terminal.difference(&baseline).collect();
    assert!(
        added.is_empty(),
        "new terminal-only commands {added:?}: every new terminal feature also gets an RPC method \
         (see AGENTS.md); classify it as Method or Client in rpc/parity.rs"
    );
    let served: Vec<_> = baseline.difference(&terminal).collect();
    assert!(
        served.is_empty(),
        "{served:?} are no longer terminal-only; remove them from TERMINAL_BASELINE"
    );
}

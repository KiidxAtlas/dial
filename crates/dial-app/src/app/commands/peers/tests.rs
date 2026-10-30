use super::*;

#[test]
fn list_and_clear_have_explicit_actions() {
    assert_eq!(arguments("  ").unwrap(), json!({"action": "list"}));
    assert_eq!(arguments("list").unwrap(), json!({"action": "list"}));
    assert_eq!(arguments("clear").unwrap(), json!({"action": "clear"}));
    assert!(arguments("list extra").is_err());
}

#[test]
fn board_maps_to_a_view_with_overlaps_marked_on_both_sides() {
    use crate::agent::BoardPeer;
    use crate::storage::coordination::Peer;
    let board = PeerBoard {
        id: "own".into(),
        summary: "parser".into(),
        phase: "working".into(),
        scopes: vec!["src".into(), "docs".into()],
        overlapping: vec!["src".into()],
        peers: vec![BoardPeer {
            peer: Peer {
                id: "peer".into(),
                session_id: "s".into(),
                summary: "client".into(),
                phase: "idle".into(),
                scopes: vec!["src/client".into(), "tests".into()],
            },
            overlapping: vec!["src/client".into()],
        }],
        queued: 3,
    };
    let view = view(board);
    assert_eq!(view.queued, 3);
    assert_eq!(view.own.id, "own");
    assert_eq!(
        view.own.claims,
        [
            Claim {
                path: "src".into(),
                overlaps: true
            },
            Claim {
                path: "docs".into(),
                overlaps: false
            }
        ]
    );
    assert_eq!(view.peers[0].phase, "idle");
    assert_eq!(view.peers[0].summary, "client");
    assert!(view.peers[0].claims[0].overlaps);
    assert!(!view.peers[0].claims[1].overlaps);
}

#[test]
fn send_preserves_message_body() {
    assert_eq!(
        arguments("send peer-123 update:  preserve 'quotes'\nand lines").unwrap(),
        json!({"action": "send", "to": "peer-123", "text": "update:  preserve 'quotes'\nand lines"})
    );
}

#[test]
fn claim_supports_quoted_paths() {
    assert_eq!(
        arguments("claim src/main.rs 'docs/a page.md'").unwrap(),
        json!({"action": "claim", "scopes": ["src/main.rs", "docs/a page.md"]})
    );
}

#[test]
fn malformed_commands_never_become_requests() {
    for args in [
        "send",
        "send peer",
        "claim",
        "claim ''",
        "claim 'broken",
        "clear extra",
        "unknown",
    ] {
        assert!(arguments(args).is_err(), "{args}");
    }
}

#[test]
fn registry_includes_peer_help_completions_and_safe_scheduling() {
    use crate::app::{command_registry, commands};
    use crate::config::Config;

    let specs: Vec<_> = commands::specs()
        .filter(|spec| spec.name == "peers")
        .collect();
    assert_eq!(specs.len(), 1);
    assert!(specs[0].args_hint.contains("send <id> <text>"));
    let completions = commands::completions(&Config::default());
    for action in ["list", "send", "claim", "clear"] {
        assert!(completions.contains(&format!("/peers {action}")));
        assert_eq!(
            command_registry::during_turn("peers", action),
            command_registry::DuringTurn::AfterTurn
        );
    }
}

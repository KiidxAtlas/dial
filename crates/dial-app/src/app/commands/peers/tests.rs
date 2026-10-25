use super::*;

#[test]
fn list_and_clear_have_explicit_actions() {
    assert_eq!(arguments("  ").unwrap(), json!({"action": "list"}));
    assert_eq!(arguments("clear").unwrap(), json!({"action": "clear"}));
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
    for action in ["send", "claim", "clear"] {
        assert!(completions.contains(&format!("/peers {action}")));
        assert_eq!(
            command_registry::during_turn("peers", action),
            command_registry::DuringTurn::AfterTurn
        );
    }
}

use super::*;
use crate::args::{Cli, Command};
use clap::Parser;
use std::io::Cursor;

#[test]
fn cli_parses_login_with_and_without_provider() {
    let bare = Cli::try_parse_from(["dial", "login"]).unwrap();
    assert!(matches!(
        bare.command,
        Some(Command::Login {
            provider: None,
            json: false
        })
    ));

    let listed = Cli::try_parse_from(["dial", "login", "--json"]).unwrap();
    assert!(matches!(
        listed.command,
        Some(Command::Login {
            provider: None,
            json: true
        })
    ));

    let target = Cli::try_parse_from(["dial", "login", "github-copilot"]).unwrap();
    assert!(matches!(
        target.command,
        Some(Command::Login {
            provider: Some(ref p),
            json: false
        }) if p == "github-copilot"
    ));
}

#[test]
fn resolve_target_handles_subscriptions_aliases_and_oauth_flows() {
    assert_eq!(resolve_target("anthropic").unwrap(), Target::Anthropic);
    assert_eq!(resolve_target("claude").unwrap(), Target::Anthropic);
    assert_eq!(resolve_target("openai").unwrap(), Target::OpenAi);
    assert_eq!(resolve_target("chatgpt").unwrap(), Target::OpenAi);
    assert_eq!(resolve_target("codex").unwrap(), Target::OpenAi);
    for flow in oauth::flows() {
        assert_eq!(
            resolve_target(flow.id).unwrap(),
            Target::OAuth(flow.id),
            "flow {}",
            flow.id
        );
    }
    let err = resolve_target("nonexistent-provider")
        .unwrap_err()
        .to_string();
    assert!(err.contains("anthropic"), "{err}");
    assert!(err.contains("openai"), "{err}");
}

#[test]
fn list_and_render_providers_cover_subscriptions_and_oauth_flows() {
    let dir = tempfile::tempdir().unwrap();
    let rows = list_providers(dir.path());
    assert!(
        rows.iter()
            .any(|r| r.provider == "anthropic" && !r.signed_in)
    );
    assert!(rows.iter().any(|r| r.provider == "openai" && !r.signed_in));
    assert!(rows.len() >= 2 + oauth::flows().count());

    let text = render_providers(&rows);
    assert!(text.contains("anthropic"), "{text}");
    assert!(text.contains("Run `dial login <provider>`"), "{text}");

    let json = json_providers(&rows);
    assert_eq!(json["providers"].as_array().unwrap().len(), rows.len());
}

#[test]
fn read_line_from_enforces_allow_empty_and_trims() {
    let mut out = Vec::new();
    let mut input = Cursor::new(b"  token-123  \n");
    let answer =
        read_line_from(&mut input, &mut out, "Paste token", Some("tok_..."), false).unwrap();
    assert_eq!(answer, "token-123");
    assert_eq!(String::from_utf8(out).unwrap(), "Paste token [tok_...]: ");

    let mut out = Vec::new();
    let mut empty = Cursor::new(b"\n");
    assert!(read_line_from(&mut empty, &mut out, "Paste token", None, false).is_err());

    let mut out = Vec::new();
    let mut empty_ok = Cursor::new(b"\n");
    assert_eq!(
        read_line_from(&mut empty_ok, &mut out, "Region", None, true).unwrap(),
        ""
    );
}

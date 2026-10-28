use super::{AccountSummary, render};
use crate::args::{AccountCommand, AuthCommand, Cli, Command};
use clap::Parser;

fn account(key: &str, label: &str, active: bool, cooling: bool) -> AccountSummary {
    AccountSummary {
        key: key.to_owned(),
        label: label.to_owned(),
        active,
        cooling,
    }
}

fn parse(args: &[&str]) -> Result<(String, Option<AccountCommand>), clap::Error> {
    let cli = Cli::try_parse_from(args)?;
    match cli.command {
        Some(Command::Auth {
            action: AuthCommand::Accounts { provider, action },
        }) => Ok((provider, action)),
        _ => panic!("expected `dial auth accounts`"),
    }
}

#[test]
fn list_marks_the_active_account_and_notes_cooling_ones() {
    let out = render(
        "acme",
        &[
            account("a1", "one@example.com", true, false),
            account("a2", "two", false, true),
        ],
    );
    assert_eq!(out, "* a1  one@example.com\n  a2  two  (cooling down)\n");
}

#[test]
fn empty_list_says_nothing_is_stored() {
    assert_eq!(render("acme", &[]), "no stored acme accounts\n");
}

#[test]
fn control_characters_cannot_split_an_account_line() {
    let out = render(
        "acme",
        &[account("a1", "bad\x1b[2Jname\nline", false, false)],
    );
    assert_eq!(out.lines().count(), 1, "{out:?}");
    assert!(!out.contains('\x1b'), "{out:?}");
}

#[test]
fn bare_accounts_command_has_no_action() {
    let (provider, action) = parse(&["dial", "auth", "accounts", "acme"]).unwrap();
    assert_eq!(provider, "acme");
    assert!(action.is_none());
}

#[test]
fn list_accepts_json() {
    let (provider, action) =
        parse(&["dial", "auth", "accounts", "acme", "list", "--json"]).unwrap();
    assert_eq!(provider, "acme");
    assert!(matches!(action, Some(AccountCommand::List { json: true })));
}

#[test]
fn select_and_remove_take_an_account_key() {
    let (_, select) = parse(&["dial", "auth", "accounts", "acme", "select", "k1"]).unwrap();
    assert!(matches!(select, Some(AccountCommand::Select { ref key }) if key == "k1"));
    let (_, remove) = parse(&["dial", "auth", "accounts", "acme", "remove", "k2"]).unwrap();
    assert!(matches!(remove, Some(AccountCommand::Remove { ref key }) if key == "k2"));
}

#[test]
fn select_and_remove_require_a_key() {
    assert!(parse(&["dial", "auth", "accounts", "acme", "remove"]).is_err());
    assert!(parse(&["dial", "auth", "accounts", "acme", "select"]).is_err());
}

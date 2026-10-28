//! `dial auth accounts <provider>`: list, select or remove the stored sign-ins of an OAuth provider.
//! The account store is shared with `/providers`; this module only reads and writes it through
//! the `oauth` helpers.

use crate::args::{AccountCommand, AuthCommand};
use anyhow::Result;
use dial::config::paths::Layout;
use dial::llm::oauth::{self, AccountSummary};
use std::fmt::Write as _;
use std::path::Path;

pub(crate) fn run(layout: &Layout, action: &AuthCommand) -> Result<()> {
    let dir = layout.user_dir.as_path();
    let AuthCommand::Accounts { provider, action } = action;
    match action {
        None => print_list(dir, provider, false),
        Some(AccountCommand::List { json }) => print_list(dir, provider, *json),
        Some(AccountCommand::Select { key }) => {
            println!("{}", oauth::select_account(dir, provider, key)?);
            Ok(())
        }
        Some(AccountCommand::Remove { key }) => {
            println!("{}", oauth::sign_out_account(dir, provider, key)?);
            Ok(())
        }
    }
}

fn print_list(dir: &Path, provider: &str, json: bool) -> Result<()> {
    let accounts = oauth::list_accounts(dir, provider)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_view(provider, &accounts))?
        );
    } else {
        print!("{}", render(provider, &accounts));
    }
    Ok(())
}

fn json_view(provider: &str, accounts: &[AccountSummary]) -> serde_json::Value {
    let accounts: Vec<serde_json::Value> = accounts
        .iter()
        .map(|account| {
            serde_json::json!({
                "key": account.key,
                "label": account.label,
                "active": account.active,
                "cooling": account.cooling,
            })
        })
        .collect();
    serde_json::json!({ "provider": provider, "accounts": accounts })
}

/// One line per account: `*` marks the active one, cooling accounts are noted. Keys and labels
/// come from stored sign-ins (emails, account ids), so control characters are replaced to keep
/// each account on one line.
pub(crate) fn render(provider: &str, accounts: &[AccountSummary]) -> String {
    if accounts.is_empty() {
        return format!("no stored {} accounts\n", clean(provider));
    }
    let mut out = String::new();
    for account in accounts {
        let marker = if account.active { "*" } else { " " };
        let cooling = if account.cooling {
            "  (cooling down)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{marker} {}  {}{cooling}",
            clean(&account.key),
            clean(&account.label)
        );
    }
    out
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests;

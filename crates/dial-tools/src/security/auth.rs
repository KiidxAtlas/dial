//! Reviewer-account selection for security operations.
//!
//! Dial's only reviewer credential is a ChatGPT OAuth account. A pinned account id must exist
//! and be signed in. Without a pin, the active signed-in account is used, or the sole signed-in
//! one. Several signed-in accounts with no pin is ambiguous and is rejected.

use std::path::Path;

use anyhow::{Result, anyhow, bail};

use crate::llm::chatgpt::{self, AccountInfo};

/// Chooses the reviewer account for a security operation from the ChatGPT credential store.
pub fn select_account(user_dir: &Path, pinned: Option<&str>) -> Result<AccountInfo> {
    choose(chatgpt::accounts(user_dir)?, pinned)
}

/// Pure selection rule, separated from the credential store for testing.
pub fn choose(accounts: Vec<AccountInfo>, pinned: Option<&str>) -> Result<AccountInfo> {
    if let Some(id) = pinned.map(str::trim).filter(|id| !id.is_empty()) {
        let account = accounts
            .into_iter()
            .find(|account| account.client_id == id)
            .ok_or_else(|| anyhow!("Unknown ChatGPT account: {id}"))?;
        if !account.signed_in {
            bail!("ChatGPT account {id} is signed out");
        }
        return Ok(account);
    }
    let mut signed_in: Vec<AccountInfo> = accounts
        .into_iter()
        .filter(|account| account.signed_in)
        .collect();
    match signed_in.len() {
        0 => bail!("Security scans require a signed-in ChatGPT account; sign in first"),
        1 => Ok(signed_in.remove(0)),
        _ => match signed_in.iter().position(|account| account.active) {
            Some(index) => Ok(signed_in.swap_remove(index)),
            None => {
                bail!("Multiple ChatGPT accounts are signed in; pin one with account=<client id>")
            }
        },
    }
}

#[cfg(test)]
mod tests;

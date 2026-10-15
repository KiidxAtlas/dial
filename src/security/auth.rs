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
mod tests {
    use super::*;

    fn account(id: &str, active: bool, signed_in: bool) -> AccountInfo {
        AccountInfo {
            client_id: id.to_owned(),
            label: id.to_owned(),
            active,
            signed_in,
        }
    }

    #[test]
    fn pinned_account_must_exist_and_be_signed_in() {
        let accounts = vec![account("a", false, true), account("b", true, false)];
        assert_eq!(choose(accounts.clone(), Some("a")).unwrap().client_id, "a");
        assert_eq!(
            choose(accounts.clone(), Some("zz"))
                .unwrap_err()
                .to_string(),
            "Unknown ChatGPT account: zz"
        );
        assert_eq!(
            choose(accounts, Some("b")).unwrap_err().to_string(),
            "ChatGPT account b is signed out"
        );
    }

    #[test]
    fn unpinned_prefers_active_then_sole_signed_in() {
        let two = vec![account("a", false, true), account("b", true, true)];
        assert_eq!(choose(two, None).unwrap().client_id, "b");
        let one = vec![account("a", false, false), account("b", false, true)];
        assert_eq!(choose(one, None).unwrap().client_id, "b");
    }

    #[test]
    fn unpinned_ambiguous_or_empty_is_rejected() {
        let ambiguous = vec![account("a", false, true), account("b", false, true)];
        assert!(choose(ambiguous, None).is_err());
        assert!(choose(vec![account("a", false, false)], None).is_err());
        assert!(choose(Vec::new(), None).is_err());
    }
}

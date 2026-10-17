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

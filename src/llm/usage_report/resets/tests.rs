use std::collections::BTreeMap;

use super::*;

fn credit(id: &str, expires_at: Option<&str>) -> UsageResetCreditDetail {
    UsageResetCreditDetail {
        id: Some(id.to_owned()),
        expires_at: expires_at.map(str::to_owned),
        ..Default::default()
    }
}

#[test]
fn parse_request_accepts_aliases_and_confirm() {
    assert_eq!(parse_request("  ").unwrap(), ResetRequest::List);
    assert_eq!(
        parse_request("claude").unwrap(),
        ResetRequest::Spend {
            provider: CLAUDE,
            confirmed: false
        }
    );
    assert_eq!(
        parse_request("Codex confirm").unwrap(),
        ResetRequest::Spend {
            provider: CODEX,
            confirmed: true
        }
    );
    assert!(parse_request("gemini").is_err());
    assert!(parse_request("codex yes").is_err());
    assert!(parse_request("codex confirm now").is_err());
}

#[test]
fn pick_credit_prefers_soonest_expiry_and_skips_unusable_grants() {
    let mut spent = credit("spent", Some("2026-10-11T00:00:00Z"));
    spent.status = Some("redeemed".to_owned());
    let mut blocked = credit("blocked", Some("2026-10-10T12:00:00Z"));
    blocked.usable = Some(false);
    let credits = vec![
        spent,
        blocked,
        credit("later", Some("2026-10-20T00:00:00Z")),
        credit("sooner", Some("2026-10-15T00:00:00Z")),
        credit("undated", None),
    ];
    assert_eq!(
        pick_credit(&credits).and_then(|c| c.id.as_deref()),
        Some("sooner")
    );
    assert_eq!(
        pick_credit(&[credit("undated", None)]).and_then(|c| c.id.as_deref()),
        Some("undated")
    );
    assert!(pick_credit(&[]).is_none());
}

#[test]
fn confirmation_names_the_confirming_command_and_scope() {
    let offer = LiveOffer {
        who: "me@example.com (Claude)".to_owned(),
        credit: None,
        reason: None,
    };
    let mut juniper = credit(JUNIPER_PROGRAM, None);
    juniper.program = Some(JUNIPER_PROGRAM.to_owned());
    juniper.title = Some("5h reset".to_owned());
    let text = confirmation_text(&offer, CLAUDE, &juniper);
    assert!(text.contains("`/usage reset anthropic confirm`"));
    assert!(text.contains("“5h reset”"));
    assert!(text.contains("weekly limits stay unchanged"));
    assert!(text.ends_with("Nothing is spent until you confirm."));
}

#[test]
fn outcome_text_follows_omp_wording() {
    let outcome = |applied, code: &str, reason: Option<&str>| ResetOutcome {
        applied,
        code: code.to_owned(),
        status: 200,
        reason: reason.map(str::to_owned),
    };
    assert_eq!(
        describe_outcome(&outcome(true, "reset", None), "me (Codex)", false),
        "Reset applied for me (Codex) — your rate-limit window has been refreshed."
    );
    assert!(
        describe_outcome(&outcome(true, "reset", None), "me (Claude)", true)
            .contains("5h session limit")
    );
    assert_eq!(
        describe_outcome(
            &outcome(false, "already_redeemed", None),
            "me (Codex)",
            false
        ),
        "me (Codex): that reset was already redeemed."
    );
    assert_eq!(
        describe_outcome(
            &outcome(false, "no_credit", Some("none banked")),
            "me (Codex)",
            false
        ),
        "me (Codex): no saved resets available to spend — none banked."
    );
    assert_eq!(
        describe_outcome(&outcome(false, "http_500", None), "me (Codex)", false),
        "me (Codex): reset was not confirmed (http_500, HTTP 200)."
    );
}

#[test]
fn list_line_reports_banked_usable_and_expiry() {
    let report = UsageReport {
        provider: CODEX.to_owned(),
        fetched_at_ms: 0,
        limits: Vec::new(),
        reset_credits: Some(UsageResetCredits {
            available_count: 2,
            redeemable_count: Some(1),
            credits: vec![
                credit("a", Some("2026-10-15T00:00:00Z")),
                credit("b", Some("2026-10-30T00:00:00Z")),
            ],
            ..Default::default()
        }),
        notes: Vec::new(),
        metadata: BTreeMap::from([("email".to_owned(), "me@example.com".to_owned())]),
    };
    assert_eq!(
        list_line(&report, CODEX),
        "- me@example.com [Codex · openai-codex]: 2 saved, 1 usable now, expires 2026-10-15T00:00:00Z"
    );
}

#[test]
fn run_without_a_signed_in_account_asks_to_login_and_spends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let error = run(dir.path(), "codex confirm", &cancel).unwrap_err();
    assert!(
        format!("{error:#}").contains("is not signed in"),
        "{error:#}"
    );
    let listing = run(dir.path(), "", &cancel).unwrap();
    assert_eq!(
        listing,
        "No provider accounts found. Use /login to add one."
    );
}

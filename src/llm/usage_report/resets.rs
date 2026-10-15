//! Saved rate-limit resets behind `/usage reset`.
//!
//! Spending is two-step: `/usage reset <provider>` reads the live offer
//! and shows what would be spent; `/usage reset <provider> confirm` spends that same grant.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow, bail};

use super::claude::{self, JUNIPER_PROGRAM};
use super::codex;
use super::factory::parse_iso_ms;
use super::fetch;
use super::types::{UsageReport, UsageResetCreditDetail, UsageResetCredits};
use crate::llm::oauth::util;

const CLAUDE: &str = "anthropic";
const CODEX: &str = "openai-codex";

/// Parsed `/usage reset` arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum ResetRequest {
    /// `/usage reset`: saved-reset balances for every signed-in provider that offers them.
    List,
    /// `/usage reset <provider>` plans; `/usage reset <provider> confirm` spends.
    Spend {
        provider: &'static str,
        confirmed: bool,
    },
}

pub fn parse_request(args: &str) -> Result<ResetRequest> {
    let mut words = args.split_whitespace();
    let Some(name) = words.next() else {
        return Ok(ResetRequest::List);
    };
    let provider = match name.to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => CLAUDE,
        "openai-codex" | "codex" => CODEX,
        other => bail!("Unknown reset provider \"{other}\". Use anthropic or openai-codex."),
    };
    let rest: Vec<&str> = words.collect();
    let confirmed = match rest.as_slice() {
        [] => false,
        ["confirm"] => true,
        _ => bail!("Usage: /usage reset [anthropic|openai-codex] [confirm]"),
    };
    Ok(ResetRequest::Spend {
        provider,
        confirmed,
    })
}

/// Entry point for `/usage reset`: runs the request against live provider state.
pub fn run(user_dir: &Path, args: &str, cancel: &Arc<AtomicBool>) -> Result<String> {
    match parse_request(args)? {
        ResetRequest::List => Ok(list(user_dir, cancel)),
        ResetRequest::Spend {
            provider,
            confirmed,
        } => {
            let offer = live_offer(user_dir, provider, cancel)?;
            let Some(credit) = offer.credit.as_ref() else {
                let reason = offer
                    .reason
                    .as_deref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default();
                return Ok(format!(
                    "{}: no saved resets usable right now{reason}.",
                    offer.who
                ));
            };
            if !confirmed {
                return Ok(confirmation_text(&offer, provider, credit));
            }
            let outcome = spend(user_dir, provider, credit, cancel)?;
            Ok(describe_outcome(
                &outcome,
                &offer.who,
                credit.program.as_deref() == Some(JUNIPER_PROGRAM),
            ))
        }
    }
}

/// The provider's current offer, read live right before any spend.
pub struct LiveOffer {
    /// Account label with provider, e.g. `me@example.com (Claude)`.
    pub who: String,
    /// The grant a confirmed spend would use; `None` when nothing is spendable.
    pub credit: Option<UsageResetCreditDetail>,
    /// Why nothing is spendable, from the provider or from local selection.
    pub reason: Option<String>,
}

pub fn live_offer(
    user_dir: &Path,
    provider: &'static str,
    cancel: &Arc<AtomicBool>,
) -> Result<LiveOffer> {
    let report = fetch(provider, user_dir, cancel)?.ok_or_else(|| {
        anyhow!(
            "{} is not signed in. Use /login to add one.",
            provider_label(provider)
        )
    })?;
    let who = format!(
        "{} ({})",
        account_label(&report, provider),
        provider_label(provider)
    );
    let resets = report.reset_credits.unwrap_or_default();
    if redeemable(&resets) == 0 {
        return Ok(LiveOffer {
            who,
            credit: None,
            reason: resets.reason,
        });
    }
    match pick_credit(&resets.credits) {
        Some(credit) => Ok(LiveOffer {
            who,
            credit: Some(credit.clone()),
            reason: None,
        }),
        None => Ok(LiveOffer {
            who,
            credit: None,
            reason: Some(
                "the provider did not identify a grant that can be safely spent".to_owned(),
            ),
        }),
    }
}

/// Result of one spend. `applied` is true only when the provider reports `reset`.
pub struct ResetOutcome {
    pub applied: bool,
    pub code: String,
    pub status: u16,
    pub reason: Option<String>,
}

/// Spends `credit` through the provider's consume endpoint. Mutations are never retried.
pub fn spend(
    user_dir: &Path,
    provider: &str,
    credit: &UsageResetCreditDetail,
    cancel: &Arc<AtomicBool>,
) -> Result<ResetOutcome> {
    let id = credit
        .id
        .as_deref()
        .ok_or_else(|| anyhow!("saved reset has no id"))?;
    match provider {
        CLAUDE => {
            let program = credit
                .program
                .as_deref()
                .ok_or_else(|| anyhow!("Claude saved reset has no program"))?;
            let outcome = claude::consume_reset(user_dir, program, id, None, cancel)?;
            Ok(ResetOutcome {
                applied: outcome.ok,
                code: outcome.code,
                status: outcome.status,
                reason: outcome.reason,
            })
        }
        CODEX => {
            let outcome = codex::consume_reset_credit(user_dir, id, &util::uuid_v4(), cancel)?;
            Ok(ResetOutcome {
                applied: outcome.ok,
                code: outcome.code,
                status: outcome.status,
                reason: None,
            })
        }
        other => bail!("provider {other:?} has no saved resets"),
    }
}

/// Saved-reset balances for each signed-in reset provider.
fn list(user_dir: &Path, cancel: &Arc<AtomicBool>) -> String {
    let mut lines = vec!["Saved rate-limit resets:".to_owned()];
    for provider in [CLAUDE, CODEX] {
        match fetch(provider, user_dir, cancel) {
            Ok(Some(report)) => lines.push(list_line(&report, provider)),
            Ok(None) => {}
            Err(error) => lines.push(format!(
                "- {} [{provider}]: unavailable ({error:#})",
                provider_label(provider)
            )),
        }
    }
    if lines.len() == 1 {
        return "No provider accounts found. Use /login to add one.".to_owned();
    }
    lines.push(String::new());
    lines.push(
        "Spend one with `/usage reset <provider>` (shows the grant), then `/usage reset <provider> confirm`."
            .to_owned(),
    );
    lines.join("\n")
}

fn list_line(report: &UsageReport, provider: &str) -> String {
    let resets = report.reset_credits.clone().unwrap_or_default();
    let usable = redeemable(&resets);
    let mut detail = format!("{} saved, {usable} usable now", resets.available_count);
    if let Some(expires) = earliest_expiry(&resets.credits) {
        detail.push_str(&format!(", expires {expires}"));
    }
    if usable == 0
        && let Some(reason) = &resets.reason
    {
        detail.push_str(&format!(" ({reason})"));
    }
    format!(
        "- {} [{} · {provider}]: {detail}",
        account_label(report, provider),
        provider_label(provider)
    )
}

/// Text shown before spending: names the grant and the exact confirming command.
pub fn confirmation_text(
    offer: &LiveOffer,
    provider: &str,
    credit: &UsageResetCreditDetail,
) -> String {
    let subject = credit
        .title
        .as_deref()
        .map_or_else(|| "1 saved reset".to_owned(), |title| format!("“{title}”"));
    let mut lines = vec![format!(
        "Run `/usage reset {provider} confirm` to spend {subject} for {}.",
        offer.who
    )];
    if credit.program.as_deref() == Some(JUNIPER_PROGRAM) {
        lines.push(
            "This resets Claude's 5h session limit only; weekly limits stay unchanged.".to_owned(),
        );
    } else if !credit.clears.is_empty() {
        lines.push(format!("Covers {}.", credit.clears.join(", ")));
    }
    if credit.requires_limit == Some(false) {
        lines.push(
            "Optional early use: this can be spent before the covered limit is reached.".to_owned(),
        );
    }
    lines.push("Nothing is spent until you confirm.".to_owned());
    lines.join(" ")
}

/// Human-facing summary of a spend outcome.
pub fn describe_outcome(outcome: &ResetOutcome, who: &str, juniper: bool) -> String {
    let reason = outcome
        .reason
        .as_deref()
        .map(|reason| format!(" — {reason}"))
        .unwrap_or_default();
    if outcome.applied {
        let scope = if juniper {
            "Claude's 5h session limit has been refreshed; weekly limits are unchanged"
        } else {
            "your rate-limit window has been refreshed"
        };
        return format!("Reset applied for {who} — {scope}.");
    }
    match outcome.code.as_str() {
        "already_redeemed" => format!("{who}: that reset was already redeemed."),
        "no_credit" => format!("{who}: no saved resets available to spend{reason}."),
        "nothing_to_reset" => format!(
            "{who}: nothing to reset right now — your covered limits aren't constrained, so no credit was spent."
        ),
        "auth_error" => format!("{who}: could not authenticate this account — try /login."),
        "malformed_response" => {
            format!(
                "{who}: couldn't confirm whether the reset applied — check /usage before trying again{reason}."
            )
        }
        code => format!(
            "{who}: reset was not confirmed ({code}, HTTP {}){reason}.",
            outcome.status
        ),
    }
}

/// Usable saved resets: the provider's advertised count, falling back to the banked count.
fn redeemable(resets: &UsageResetCredits) -> u64 {
    resets.redeemable_count.unwrap_or(resets.available_count)
}

/// A grant that can still be spent: has an id, is not marked unusable, and is `available`.
fn credit_usable(credit: &UsageResetCreditDetail) -> bool {
    credit.id.is_some()
        && credit.usable != Some(false)
        && credit.status.as_deref().unwrap_or("available") == "available"
}

/// Soonest-expiring usable grant with an id; undated grants only when none are dated.
fn pick_credit(credits: &[UsageResetCreditDetail]) -> Option<&UsageResetCreditDetail> {
    let mut best: Option<(&UsageResetCreditDetail, u64)> = None;
    let mut undated: Option<&UsageResetCreditDetail> = None;
    for credit in credits {
        let usable = credit_usable(credit);
        if !usable {
            continue;
        }
        match credit.expires_at.as_deref().and_then(parse_iso_ms) {
            Some(expiry) if best.is_none_or(|(_, soonest)| expiry < soonest) => {
                best = Some((credit, expiry))
            }
            Some(_) => {}
            None => {
                undated.get_or_insert(credit);
            }
        }
    }
    best.map(|(credit, _)| credit).or(undated)
}

fn earliest_expiry(credits: &[UsageResetCreditDetail]) -> Option<&str> {
    credits
        .iter()
        .filter(|credit| credit_usable(credit))
        .filter_map(|credit| credit.expires_at.as_deref())
        .min_by_key(|expiry| parse_iso_ms(expiry).unwrap_or(u64::MAX))
}

fn provider_label(provider: &str) -> &'static str {
    if provider == CODEX { "Codex" } else { "Claude" }
}

fn account_label(report: &UsageReport, provider: &str) -> String {
    report
        .metadata
        .get("email")
        .or_else(|| report.metadata.get("account"))
        .cloned()
        .unwrap_or_else(|| provider_label(provider).to_owned())
}

#[cfg(test)]
mod tests {
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
}

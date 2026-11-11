use super::*;
use agent::safety::approval::{ApprovalPreview, PolicyExplanation, PolicyReason};

const HINT: &str = "n or Esc denies · Ctrl+C denies and stops the turn";

#[test]
fn reusable_choices_lead_with_the_action_and_carry_lifetimes_in_labels() {
    let preview = ApprovalPreview {
        once: "Only this call, once".into(),
        session: Some(
            "bash normalized argv starts with [cargo, test]. Lifetime: rest of session, not saved."
                .into(),
        ),
        project: Some(
            "bash normalized argv starts with [cargo, test]. Lifetime: future sessions until revoked."
                .into(),
        ),
        unavailable_reason: None,
        undo: "Checkpointed: /undo restores the previous file contents.".into(),
    };
    let prompt = approval_prompt("cargo test foo", &preview, HINT);
    assert!(prompt.body.starts_with("cargo test foo"));
    // Reversibility is stated directly under the action, before any scope text.
    assert!(
        prompt
            .body
            .starts_with("cargo test foo\n\nCheckpointed: /undo restores"),
        "{}",
        prompt.body
    );
    assert!(
        prompt
            .body
            .contains("project covers: bash normalized argv starts with [cargo, test].")
    );
    assert!(!prompt.body.contains("Lifetime:") && !prompt.body.contains("Policy:"));
    assert!(prompt.body.ends_with(HINT));
    assert_eq!(
        prompt
            .options
            .iter()
            .map(|(label, key, decision)| (*key, *decision, label.contains('(')))
            .collect::<Vec<_>>(),
        vec![
            ('y', Decision::Allow, false),
            ('a', Decision::AllowAlways, true),
            ('p', Decision::AllowProject, true),
            ('n', Decision::Deny, false)
        ]
    );
}

#[test]
fn unavailable_scopes_cannot_be_selected_or_promised() {
    let preview = ApprovalPreview {
        once: "Only this exact call".into(),
        session: None,
        project: None,
        unavailable_reason: Some("Compound commands require fresh approval".into()),
        undo: String::new(),
    };
    let prompt = approval_prompt("cargo test && echo done", &preview, HINT);
    assert!(
        prompt
            .body
            .contains("Compound commands require fresh approval")
    );
    assert!(prompt.scope.is_none() && !prompt.body.contains("project covers"));
    assert_eq!(
        prompt
            .options
            .iter()
            .map(|(_, _, d)| *d)
            .collect::<Vec<_>>(),
        vec![Decision::Allow, Decision::Deny]
    );
}

#[test]
fn batch_prompt_lists_one_row_per_change_and_never_promises_a_single_call() {
    let rows = vec![
        "write src/a.rs · 3 lines, 40 B".to_owned(),
        "edit src/b.rs · 2 new lines".to_owned(),
    ];
    let body = batch_prompt(&rows, HINT);
    assert!(body.starts_with("2 changes are ready"));
    assert!(rows.iter().all(|row| body.contains(row.as_str())));
    assert!(!body.contains("Only this call"));
    assert!(body.ends_with(HINT));
}

#[test]
fn a_project_grant_names_its_match_project_lifetime_and_limits() {
    let body = project_grant_body(
        Some("bash normalized argv starts with [cargo, test]."),
        std::path::Path::new("/work/demo"),
    );
    for part in [
        "Covers: bash normalized argv starts with [cargo, test].",
        "every future session in /work/demo",
        "until you revoke it in /permissions",
        "do not match still ask",
    ] {
        assert!(body.contains(part), "{part}: {body}");
    }
    assert!(
        project_grant_body(None, std::path::Path::new("/w")).contains("calls matching this one")
    );
}

#[test]
fn ask_user_hotkeys_stop_at_nine() {
    let keys: Vec<char> = (0..11).map(ask_user_hotkey).collect();
    assert_eq!(&keys[..9], &['1', '2', '3', '4', '5', '6', '7', '8', '9']);
    assert_eq!(&keys[9..], &[' ', ' ']);
}

#[test]
fn policy_denials_use_user_wording_and_quiet_bookkeeping() {
    let explain = |reason| PolicyExplanation {
        reason,
        rule_scope: None,
    };
    let plan = policy_notice("write", &explain(PolicyReason::PlanMode)).unwrap();
    assert!(plan.contains("plan mode") && !plan.contains("read-only mode"));
    assert!(!plan.contains("Policy:"));
    for reason in [
        PolicyReason::DenyRule,
        PolicyReason::ReadOnly,
        PolicyReason::HookVeto,
    ] {
        assert!(policy_notice("bash", &explain(reason)).is_some());
    }
    for reason in [
        PolicyReason::Cancelled,
        PolicyReason::OperatorDeny,
        PolicyReason::PromptRequired,
        PolicyReason::SafeCall,
    ] {
        assert!(policy_notice("bash", &explain(reason)).is_none());
    }
}

#[test]
fn policy_log_rows_name_the_deciding_rule_and_the_control_that_changes_it() {
    let row = |reason, scope: Option<&str>| {
        policy_log_row(
            "bash",
            &PolicyExplanation {
                reason,
                rule_scope: scope.map(str::to_owned),
            },
        )
    };
    assert_eq!(
        row(PolicyReason::AllowRule, Some("bash cargo test")),
        "bash: Policy: allowed by a saved permission rule (bash cargo test). To change: remove the rule with /permissions remove <number>."
    );
    assert_eq!(
        row(PolicyReason::AutomaticMode, None),
        "bash: Policy: allowed by automatic approval mode. To change: choose /approval ask or /approval write to be prompted."
    );
    assert!(row(PolicyReason::ReadOnly, None).ends_with("leave read-only mode with /approval."));
}

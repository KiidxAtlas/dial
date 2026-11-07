use super::*;
use agent::safety::approval::ApprovalPreview;

#[test]
fn reusable_choices_show_policy_scope_and_persistence_before_consent() {
    let preview = ApprovalPreview {
        once: "Only this call, once".into(),
        session: Some(
            "bash normalized argv starts with [cargo, test]; rest of session, not saved".into(),
        ),
        project: Some(
            "bash normalized argv starts with [cargo, test]; future sessions until revoked".into(),
        ),
        unavailable_reason: None,
    };
    let prompt = approval_prompt("cargo test foo", &preview);
    assert!(prompt.body.contains(preview.session.as_ref().unwrap()));
    assert!(prompt.body.contains(preview.project.as_ref().unwrap()));
    assert_eq!(
        prompt
            .options
            .iter()
            .map(|(_, _, d)| *d)
            .collect::<Vec<_>>(),
        vec![
            Decision::Allow,
            Decision::AllowAlways,
            Decision::AllowProject,
            Decision::Deny
        ]
    );
    assert_eq!(
        prompt
            .options
            .iter()
            .map(|(_, key, _)| *key)
            .collect::<Vec<_>>(),
        vec!['y', 'a', 'p', 'n']
    );
}

#[test]
fn unavailable_scopes_cannot_be_selected_or_promised() {
    let preview = ApprovalPreview {
        once: "Only this exact call".into(),
        session: None,
        project: None,
        unavailable_reason: Some("Compound commands require fresh approval".into()),
    };
    let prompt = approval_prompt("cargo test && echo done", &preview);
    assert!(
        prompt
            .body
            .contains("Compound commands require fresh approval")
    );
    assert!(!prompt.body.contains("Session:") && !prompt.body.contains("Project:"));
    assert_eq!(
        prompt
            .options
            .iter()
            .map(|(_, _, d)| *d)
            .collect::<Vec<_>>(),
        vec![Decision::Allow, Decision::Deny]
    );
}

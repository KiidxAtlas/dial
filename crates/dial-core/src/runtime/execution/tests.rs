use super::*;

#[test]
fn recovery_derives_retry_and_next_action_from_status_and_effects() {
    use crate::runtime::execution::{NextAction, Recovery};
    // (status, effects, expected retry, expected next action)
    let cases = [
        (
            ToolStatus::Error,
            Effects::None,
            Retry::AfterCorrection,
            NextAction::Correct,
        ),
        (
            ToolStatus::Error,
            Effects::Known,
            Retry::AfterCorrection,
            NextAction::Correct,
        ),
        (
            ToolStatus::Blocked,
            Effects::None,
            Retry::Withheld,
            NextAction::RespectBoundary,
        ),
        (
            ToolStatus::Aborted,
            Effects::Known,
            Retry::OnlyByDirection,
            NextAction::FollowDirection,
        ),
        (
            ToolStatus::Timeout,
            Effects::Known,
            Retry::AfterInspection,
            NextAction::InspectProcess,
        ),
        (
            ToolStatus::Skipped,
            Effects::None,
            Retry::AfterCorrection,
            NextAction::ResolvePrerequisite,
        ),
        (
            ToolStatus::Ok,
            Effects::Unknown,
            Retry::AfterInspection,
            NextAction::InspectState,
        ),
        (
            ToolStatus::Timeout,
            Effects::Unknown,
            Retry::AfterInspection,
            NextAction::InspectState,
        ),
        (
            ToolStatus::Blocked,
            Effects::Unknown,
            Retry::AfterInspection,
            NextAction::InspectState,
        ),
    ];
    for (status, effects, retry, next) in cases {
        let recovery: Recovery = Recovery::derive(status, effects, 0).unwrap();
        assert_eq!(
            (recovery.retry, recovery.next),
            (retry, next),
            "{status:?}/{effects:?}"
        );
    }
    assert!(Recovery::derive(ToolStatus::Ok, Effects::Known, 0).is_none());
    assert!(Recovery::derive(ToolStatus::Ok, Effects::None, 0).is_none());
}

#[test]
fn recovery_counts_distinct_committed_paths_and_ignores_prose() {
    let execution = Execution {
        status: Some(ToolStatus::Error),
        effects: Effects::Known,
        changed_paths: vec!["a.rs".into(), "a.rs".into(), "b.rs".into()],
        ..Default::default()
    };
    let recovery = execution.recovery(ToolStatus::Error).unwrap();
    assert_eq!(recovery.committed_paths, 2);
    assert_eq!(recovery.effects, Effects::Known);
}

#[test]
fn legacy_execution_without_recovery_fields_still_derives_recovery() {
    let facts: Execution =
        serde_json::from_str(r#"{"status":"timeout","effects":"unknown","changed_paths":[]}"#)
            .unwrap();
    let recovery = facts.recovery(ToolStatus::Timeout).unwrap();
    assert_eq!(recovery.retry, Retry::AfterInspection);
    assert_eq!(
        recovery.next,
        crate::runtime::execution::NextAction::InspectState
    );
}
#[test]
fn legacy_execution_deserializes_without_new_facts() {
    let facts: Execution =
        serde_json::from_str(r#"{"status":"ok","effects":"known","changed_paths":[]}"#).unwrap();
    assert!(facts.observation.is_none());
    assert!(facts.process.is_none());
    assert!(facts.output.is_none());
    assert!(facts.shell_launch.is_none());
    assert!(
        serde_json::to_value(&facts)
            .unwrap()
            .get("shell_launch")
            .is_none()
    );
    assert!(facts.integration.is_none());
    assert!(facts.native_edit_retry.is_none());
}

#[test]
fn presentation_clipping_preserves_capture_and_retention() {
    let mut facts = OutputIntegrity {
        capture: Completeness::Complete,
        retention: Completeness::Complete,
        presentation: Completeness::Complete,
        retained_bytes: Some(20),
        presented_bytes: Some(20),
        ..Default::default()
    };
    facts.mark_presentation_partial();
    assert_eq!(facts.capture, Completeness::Complete);
    assert_eq!(facts.retention, Completeness::Complete);
    assert_eq!(facts.retained_bytes, Some(20));
    assert_eq!(facts.presentation, Completeness::Partial);
    assert_eq!(facts.presented_bytes, None);
}

#[test]
fn shell_launch_execution_round_trips_without_a_process_outcome() {
    for stdin in [ShellStdin::Eof, ShellStdin::Pipe, ShellStdin::Terminal] {
        let facts = Execution {
            shell_launch: Some(ShellLaunch {
                stdin,
                terminal_allocated: stdin == ShellStdin::Terminal,
                job: Some("job-1".into()),
            }),
            ..Default::default()
        };
        let restored: Execution =
            serde_json::from_value(serde_json::to_value(&facts).unwrap()).unwrap();
        assert_eq!(restored, facts);
        assert!(restored.process.is_none());
    }
}

#[test]
fn native_witness_requirements_survive_nested_failures_but_not_recovered_calls() {
    let requirement = NativeWitnessRequirement {
        path: "source.rs".into(),
        source_sha256: [1; 32],
        raw_revision: None,
        ranges: vec![(2, 4)],
    };
    let outer = Capture::begin();
    let inner = Capture::begin();
    native_edit_retry(requirement.clone());
    let failed = inner.finish(false);
    assert_eq!(
        failed.native_edit_retry.unwrap().requirements,
        vec![requirement.clone()]
    );
    let captured = outer.finish(false);
    assert_eq!(
        captured.native_edit_retry.unwrap().requirements,
        vec![requirement.clone()]
    );

    let capture = Capture::begin();
    native_edit_retry(requirement);
    assert!(capture.finish(true).native_edit_retry.is_none());
}

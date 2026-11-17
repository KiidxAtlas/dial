use super::*;

#[test]
fn strict_requests_refuse_ignored_fields_and_invalid_values() {
    for request in [
        json!({"mode":"vibe","enabled":true,"confirm":true}),
        json!({"mode":"plan","enabled":"yes"}),
        json!({"mode":"prewalk","action":"toggle"}),
        json!({"mode":"unknown","enabled":true}),
    ] {
        assert!(parse::<ModeRequest>(&request).is_err(), "{request}");
    }
    assert!(parse::<SideRequest>(&json!({"action":"cancel","thread_id":"1"})).is_err());
}

#[test]
fn goal_lifecycle_preserves_budget_and_refuses_completion_bypass() {
    let mut state = crate::agent::modes::goal::State::default();
    let prompt = apply_goal(
        &mut state,
        &GoalRequest::Set {
            objective: "ship".into(),
            token_budget: Some(10),
        },
    )
    .unwrap();
    assert_eq!(prompt.as_deref(), Some("ship"));
    state.account(10, 0);
    assert_eq!(
        state.goal.as_ref().unwrap().status,
        crate::agent::modes::goal::Status::BudgetLimited
    );
    let prompt = apply_goal(
        &mut state,
        &GoalRequest::Budget {
            token_budget: Some(20),
        },
    )
    .unwrap();
    assert_eq!(prompt.as_deref(), Some("/goal budget 20"));
    apply_goal(&mut state, &GoalRequest::Pause {}).unwrap();
    assert!(
        apply_goal(
            &mut state,
            &GoalRequest::Guided {
                idea: "replace".into()
            }
        )
        .is_err()
    );
    apply_goal(&mut state, &GoalRequest::Resume {}).unwrap();
    apply_goal(&mut state, &GoalRequest::Drop {}).unwrap();
    assert!(apply_goal(&mut state, &GoalRequest::Resume {}).is_err());
    assert!(apply_goal(&mut state, &GoalRequest::Budget { token_budget: None }).is_err());
    assert!(parse::<GoalRequest>(&json!({"action":"complete"})).is_err());
}

#[test]
fn invalid_goal_inputs_do_not_change_the_objective() {
    let mut state = crate::agent::modes::goal::State::default();
    state.start("original", Some(10));
    for request in [
        GoalRequest::Set {
            objective: " ".into(),
            token_budget: None,
        },
        GoalRequest::Set {
            objective: "replacement".into(),
            token_budget: Some(0),
        },
        GoalRequest::Budget {
            token_budget: Some(0),
        },
    ] {
        assert!(apply_goal(&mut state, &request).is_err());
        assert_eq!(state.goal.as_ref().unwrap().objective, "original");
        assert_eq!(state.goal.as_ref().unwrap().token_budget, Some(10));
    }
}

#[test]
fn side_management_needs_no_live_agent_and_missing_context_is_coded() {
    let mut state = SideThreadState::default();
    assert_eq!(
        side(&mut state, None, &json!({"action":"list"})).unwrap()["threads"],
        json!([])
    );
    assert_eq!(
        side(&mut state, None, &json!({"action":"cancel"})).unwrap()["cancelled"],
        false
    );
    for (params, expected) in [
        (json!({"action":"ask","question":"why"}), ErrorCode::Busy),
        (
            json!({"action":"ask","question":" "}),
            ErrorCode::InvalidParams,
        ),
        (
            json!({"action":"open","thread_id":"missing"}),
            ErrorCode::NotFound,
        ),
        (json!({"action":"answer"}), ErrorCode::NotFound),
    ] {
        let error = side(&mut state, None, &params).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            expected,
            "{params}"
        );
    }
}

#[test]
fn persistent_modes_preserve_scope_and_tier_ownership() {
    use crate::config::settings_store::{Change, Scope};
    let config = crate::config::Config {
        provider: Some("openai".into()),
        slow_mode: true,
        ..Default::default()
    };
    for (request, scope, path) in [
        (
            json!({"mode":"drafter","enabled":true}),
            Scope::Project,
            "experimental.turn_optimizer",
        ),
        (
            json!({"mode":"extended_context","enabled":false}),
            Scope::Global,
            "extended_context",
        ),
        (
            json!({"mode":"fast","state":"on"}),
            Scope::Global,
            "fast_mode",
        ),
        (
            json!({"mode":"fast","state":"ultra"}),
            Scope::Global,
            "service_tier",
        ),
        (
            json!({"mode":"slow","enabled":false}),
            Scope::Global,
            "slow_mode",
        ),
    ] {
        let (actual_scope, changes) = mode_settings(&config, &request).unwrap().unwrap();
        assert_eq!(actual_scope, scope, "{request}");
        assert_eq!(changes.len(), 1, "{request}");
        assert!(matches!(&changes[0], Change::Set { path: actual, .. } if actual == path));
    }
    assert!(
        mode_settings(&config, &json!({"mode":"advisor","state":"completion"}))
            .unwrap()
            .is_none()
    );
}

#[test]
fn unsupported_tiers_are_coded_refusals_and_off_is_always_available() {
    for (provider, request) in [
        ("mlx", json!({"mode":"fast","state":"on"})),
        ("anthropic", json!({"mode":"fast","state":"ultra"})),
        ("anthropic", json!({"mode":"slow","enabled":true})),
    ] {
        let config = crate::config::Config {
            provider: Some(provider.into()),
            ..Default::default()
        };
        let error = mode_settings(&config, &request).err().unwrap();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            ErrorCode::Refused
        );
        for off in [
            json!({"mode":"fast","state":"off"}),
            json!({"mode":"slow","enabled":false}),
        ] {
            assert!(mode_settings(&config, &off).unwrap().is_some());
        }
    }
}

#[test]
fn mode_preferences_reject_malformed_requests_before_planning() {
    let config = crate::config::Config::default();
    for request in [
        json!({"mode":"drafter","enabled":true,"scope":"global"}),
        json!({"mode":"fast","enabled":true}),
        json!({"mode":"fast","state":"priority"}),
        json!({"mode":"advisor","state":"toggle"}),
        json!({"mode":"external_thinking","enabled":"yes"}),
        json!({"mode":"extended_context"}),
    ] {
        let error = mode_settings(&config, &request).err().unwrap();
        assert_eq!(
            error
                .downcast_ref::<super::super::RequestError>()
                .unwrap()
                .code,
            ErrorCode::InvalidParams,
            "{request}"
        );
    }
}

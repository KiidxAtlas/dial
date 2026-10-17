use super::workflow_evidence;
use serde_json::json;

#[test]
fn workflow_hypotheses_require_distinct_active_version_trajectories() {
    let protocol = json!({"kind":"tool_form_failure","data":{"kind":"schema"}});
    let read = json!({"kind":"tool_call","data":{"name":"read"}});
    let mut events = vec![read; 8];
    events.extend(vec![protocol.clone(); 5]);
    let observations = json!([
        {"version":7,"events":events},
        {"version":6,"events":[protocol.clone()]},
        {"version":7,"events":[protocol]},
        {"version":7,"events":[]}
    ]);
    let result = workflow_evidence(&observations, 7);
    assert_eq!(result.trajectories, 2);
    assert_eq!(result.protocol_trajectories, 2);
    assert_eq!(result.investigation_trajectories, 1);
    assert_eq!(result.failure_kinds.get("schema"), Some(&6));
    assert_eq!(result.observed.protocol_errors, 6);
    assert_eq!(result.observed.tool_calls, 8);
}

#[test]
fn malformed_events_do_not_become_recurring_investigation() {
    let result = workflow_evidence(
        &json!([
            {"version":1,"events":[{},{"kind":"generation","data":{"tool_calls":"bad"}}]},
            {"version":1,"events":"bad"},
            {"events":[{"kind":"tool_form_failure"}]}
        ]),
        1,
    );
    assert_eq!(result.trajectories, 1);
    assert_eq!(result.investigation_trajectories, 0);
    assert_eq!(result.protocol_trajectories, 0);
}

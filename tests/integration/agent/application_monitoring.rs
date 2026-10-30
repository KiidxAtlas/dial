//! Real file watch -> approved tool -> idle reservation -> scripted repair, without a live model.
use super::*;

#[test]
fn approved_application_monitor_wakes_and_repairs_under_existing_permissions() {
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["monitor"]})),
        Reply::Call(
            "monitor",
            json!({"action":"start","name":"site","path":"site.log"}),
        ),
        Reply::Text("Watching for new errors."),
        Reply::Call("read", json!({"path":"app.txt"})),
        Reply::Call("write", json!({"path":"app.txt","content":"fixed\n"})),
        Reply::Text("Fixed the observed error."),
        Reply::Text("Fixed the observed error; patch reviewed."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.integrations = vec!["monitor".into()];
    e.config.coordination.enabled = false;
    std::fs::write(e.project.path().join("site.log"), "old error\n").unwrap();
    e.config.verification.enabled = false;
    std::fs::write(e.project.path().join("app.txt"), "broken\n").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.set_skillful(Some(false)).unwrap();
    let mut ui = Recorder::default();
    let initial = a
        .turn(
            "Monitor site.log and fix new runtime errors in app.txt.",
            &mut ui,
        )
        .unwrap();
    assert_eq!(initial.outcome, TurnOutcome::Completed, "{}", ui.text);
    assert_eq!(
        ui.approvals,
        ["monitor"],
        "Auto mode must still ask before arming"
    );
    assert!(!a.monitor_wake_pending(), "historical error must not wake");
    let before = a.session().control_state();
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(e.project.path().join("site.log"))
        .unwrap();
    writeln!(log, "Error: app.txt contains broken value").unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !a.monitor_wake_pending() {
        assert!(
            Instant::now() < deadline,
            "new error did not reserve a wake"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let result = a.resume_from_monitor(&mut ui).unwrap();
    assert_eq!(result.outcome, TurnOutcome::Completed, "{}", ui.text);
    assert_eq!(
        std::fs::read_to_string(e.project.path().join("app.txt")).unwrap(),
        "fixed\n"
    );
    assert_eq!(
        a.session().control_state()["current_instruction_entry_id"],
        before["current_instruction_entry_id"]
    );
    assert_eq!(
        a.session().control_state()["current_instruction"],
        before["current_instruction"]
    );
    assert!(!a.monitor_wake_pending(), "one-shot watch must disarm");
    let requests = server.requests.lock();
    let wake = requests[3]["messages"].to_string();
    assert!(wake.contains("app.txt contains broken value"));
    assert!(
        !ui.results.iter().any(|(_, _, failed)| *failed),
        "{:?}",
        ui.results
    );
}

#[test]
fn denied_application_monitor_never_arms_in_auto_mode() {
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["monitor"]})),
        Reply::Call(
            "monitor",
            json!({"action":"start","name":"site","path":"site.log"}),
        ),
        Reply::Text("Monitoring was declined."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.integrations = vec!["monitor".into()];
    std::fs::write(e.project.path().join("site.log"), "").unwrap();
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    let mut ui = Recorder {
        deny: true,
        ..Recorder::default()
    };
    a.turn("Monitor site.log for runtime errors.", &mut ui)
        .unwrap();
    assert_eq!(
        ui.approvals,
        ["monitor"],
        "calls={:?}, results={:?}, notices={:?}",
        ui.calls,
        ui.results,
        ui.notices
    );
    assert!(!a.monitor_wake_pending());
    assert!(a.monitor_wakeup_interval().is_none());
    assert!(a.resume_from_monitor(&mut ui).is_err());
}

#[test]
fn application_monitor_is_revoked_before_attached_root_removal() {
    let attached = tempfile::tempdir().unwrap();
    let path = attached.path().join("site.log");
    std::fs::write(&path, "").unwrap();
    let server = serve(vec![
        Reply::Call("tool_load", json!({"names":["monitor"]})),
        Reply::Call(
            "monitor",
            json!({"action":"start","name":"site","path":path}),
        ),
        Reply::Text("Watching the attached log."),
    ]);
    let mut e = env(&server.url, "auto");
    e.config.integrations = vec!["monitor".into()];
    let mut a = agent_with(&e, None, Arc::new(AtomicBool::new(false)));
    a.set_skillful(Some(false)).unwrap();
    a.add_workspace_roots(&[attached.path().to_path_buf()])
        .unwrap();
    let mut ui = Recorder::default();
    a.turn("Monitor the attached site log for runtime errors.", &mut ui)
        .unwrap();
    assert_eq!(ui.approvals, ["monitor"]);
    assert!(a.monitor_wakeup_interval().is_some());
    assert!(a.workspace_mut().remove_root(attached.path()).unwrap());
    std::fs::write(&path, "Error: after access was revoked\n").unwrap();
    assert!(!a.monitor_wake_pending());
    assert!(a.monitor_wakeup_interval().is_none());
    assert!(a.resume_from_monitor(&mut ui).is_err());
}

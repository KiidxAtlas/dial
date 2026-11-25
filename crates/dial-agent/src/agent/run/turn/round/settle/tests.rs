use super::*;
use crate::runtime::execution::ProcessOutcome;
use crate::runtime::tool_output::ToolOutput;

#[test]
fn detached_launch_clears_stopped_shell_job_and_unattended_session_job_warns_immediately() {
    let started = || {
        ToolOutput::ok("ok".into()).with_process(ProcessOutcome {
            started: true,
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            cancelled: false,
            output_complete: true,
        })
    };
    let call = |name: &str, arguments: serde_json::Value| ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments,
        malformed_arguments: false,
    };

    let mut slot = None;
    // Plain `&` sets the stopped-job slot.
    track_stopped_shell_job(
        &mut slot,
        &call("bash", json!({"command": "python3 server.py &"})),
        &started(),
    );
    assert!(slot.as_deref().is_some_and(|s| s.contains("server.py")));
    // A subsequent `setsid ... &` clears it and does not re-arm it.
    track_stopped_shell_job(
        &mut slot,
        &call(
            "bash",
            json!({"command": "setsid nohup python3 server.py >/tmp/s.log 2>&1 & disown"}),
        ),
        &started(),
    );
    assert_eq!(slot, None);

    // Immediate warning fires in unattended runs for `bash background:true` and non-durable `bg start`.
    let mut bash_bg = ToolOutput::ok("started".into());
    bash_bg.data = Some(json!({"background_job": "bg1"}));
    let note = unattended_session_job_note(
        true,
        &call("bash", json!({"command": "qemu ...", "background": true})),
        &bash_bg,
    )
    .expect("unattended bash background:true should warn");
    assert!(
        note.contains("bg1") && note.contains("persist=true name=<name> (without `ready`)"),
        "{note}"
    );
    assert!(
        unattended_session_job_note(
            false,
            &call("bash", json!({"command": "qemu ...", "background": true})),
            &bash_bg,
        )
        .is_none()
    );

    let mut bg_start = ToolOutput::ok("started".into());
    bg_start.data = Some(json!({"job": {"name": "svc"}}));
    assert!(
        unattended_session_job_note(
            true,
            &call("bg", json!({"action": "start", "command": "srv"})),
            &bg_start,
        )
        .is_some()
    );
    assert!(
        unattended_session_job_note(
            true,
            &call(
                "bg",
                json!({"action": "start", "persist": true, "name": "svc", "command": "srv"})
            ),
            &bg_start,
        )
        .is_none()
    );
}

use super::*;
#[test]
fn a_caught_reaped_timeout_is_an_ordinary_handled_failure() {
    let output = run(&AtomicBool::new(false), || {
        let _ = run(&AtomicBool::new(false), || {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", "sleep 5"]);
            let finished = crate::runtime::process::run(
                command,
                None,
                std::time::Duration::from_millis(20),
                128,
                &AtomicBool::new(false),
                &mut |_, _| {},
            )
            .unwrap();
            assert!(finished.timed_out);
            ToolOutput::ok("caller caught the physical timeout".into())
        });
        ToolOutput::ok("outer caller also returned success".into())
    });
    // The group was killed and reaped, so nothing is uncertain: like a caught non-zero
    // exit, the handling tool's own result stands and the turn is not stopped.
    assert_eq!(output.execution.effects, Effects::Known);
    assert!(!output.is_error);
}

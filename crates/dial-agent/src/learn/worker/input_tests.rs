use super::*;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

fn fixture_args(name: &str) -> Vec<String> {
    vec![
        "--exact".into(),
        format!("learn::worker::input_tests::{name}"),
        "--ignored".into(),
        "--nocapture".into(),
    ]
}

// Spawn this test binary instead of requiring an installed Python or shell.
#[test]
#[ignore = "subprocess fixture: reads stdin to EOF"]
fn eof_child() {
    let noise = vec![b'x'; 128 * 1024];
    std::io::stdout().write_all(&noise).unwrap();
    std::io::stderr().write_all(&noise).unwrap();
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    assert!(input.iter().all(|byte| *byte == b'q'));
    println!("request-bytes={}", input.len());
}

#[test]
#[ignore = "subprocess fixture: deliberately never consumes stdin"]
fn blocked_child() {
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
fn handoff_closes_input_before_exit_and_drains_output() {
    let program = std::env::current_exe().unwrap();
    for size in [0, 512 * 1024] {
        let output =
            run_command(&program, &fixture_args("eof_child"), &"q".repeat(size), 10).unwrap();
        assert!(output.contains(&format!("request-bytes={size}")));
        assert!(output.contains(&"x".repeat(128 * 1024)));
    }
}

#[test]
fn handoff_blocked_input_obeys_timeout() {
    let started = Instant::now();
    let error = run_command(
        &std::env::current_exe().unwrap(),
        &fixture_args("blocked_child"),
        &"q".repeat(512 * 1024),
        1,
    )
    .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn handoff_blocked_input_obeys_cancellation() {
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            cancel.store(true, Ordering::SeqCst);
        });
        let error = run_command_cancellable(
            &std::env::current_exe().unwrap(),
            &fixture_args("blocked_child"),
            &"q".repeat(512 * 1024),
            10,
            &cancel,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
    });
    assert!(started.elapsed() < Duration::from_secs(5));
}

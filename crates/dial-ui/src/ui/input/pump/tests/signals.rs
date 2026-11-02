//! Process-isolated checks: a signal must wake input already waiting, not only new calls.
use super::*;
use crate::platform::{process, terminal::Signals};
use std::{
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    time::Instant,
};

const LIMIT: Duration = Duration::from_secs(2);

/// An empty terminal whose first poll is observable without using elapsed time as readiness.
struct Empty {
    entered: Option<Sender<Duration>>,
    quiet: Receiver<()>,
    _keep_open: Sender<()>,
}

impl Empty {
    fn new() -> (Self, Receiver<Duration>) {
        let (entered, notice) = mpsc::channel();
        let (keep_open, quiet) = mpsc::channel();
        (
            Self {
                entered: Some(entered),
                quiet,
                _keep_open: keep_open,
            },
            notice,
        )
    }
}

impl EventSource for Empty {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        if let Some(entered) = self.entered.take() {
            entered.send(timeout).expect("observe first poll");
        }
        match self.quiet.recv_timeout(timeout) {
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(false),
            other => panic!("empty terminal unexpectedly became ready: {other:?}"),
        }
    }

    fn read(&mut self) -> io::Result<Event> {
        panic!("an empty terminal must never be read without readiness")
    }
}

fn wait_for_consumer(pump: &Pump) {
    let deadline = Instant::now() + LIMIT;
    let mut state = pump.state();
    while state.waiting == 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "consumer did not reach its empty-queue wait"
        );
        state = pump
            .changed
            .wait_timeout(state, remaining)
            .expect("observe consumer wait")
            .0;
    }
    assert!(
        state.running && state.events.is_empty(),
        "consumer must be waiting on the running, empty pump"
    );
    // The consumer set waiting under this mutex. Acquiring it proves the consumer released
    // it in Condvar::wait_timeout, so sending the signal now cannot test only entry checks.
}

#[test]
fn termination_input_child() {
    let Ok(case) = std::env::var("DIAL_TEST_INPUT_TERMINATION") else {
        return;
    };
    let (mode, signal) = case.split_once(':').expect("mode:signal");
    let signal: i32 = signal.parse().expect("signal number");
    let _signals = Signals::install_termination().expect("install child signal bridge");
    let pump = pump();
    let running = mode.starts_with("running");
    if running {
        let (source, polled) = Empty::new();
        pump.start(Box::new(source)).expect("start empty pump");
        assert!(polled.recv_timeout(LIMIT).expect("reader has entered poll") <= TICK);
    }
    let (mut idle, idle_polled) = Empty::new();
    let (done, completion) = mpsc::channel();
    let consumer = Arc::clone(&pump);
    let polling = mode.ends_with("poll");
    let reader = thread::spawn(move || {
        let outcome = if polling {
            consumer
                .poll_with(Duration::from_secs(30), &mut idle)
                .map(|_| ())
        } else {
            consumer.read_with(&mut idle).map(|_| ())
        };
        done.send(outcome).expect("report input completion");
    });
    if running {
        wait_for_consumer(&pump);
    } else {
        let timeout = idle_polled
            .recv_timeout(LIMIT)
            .expect("stopped consumer is inside idle poll");
        assert!(
            timeout <= TICK,
            "idle poll must be bounded, got {timeout:?}"
        );
    }
    // SAFETY: only this isolated child receives the signal; its bridge is already installed.
    let result = unsafe { libc::raise(signal) };
    assert_eq!(result, 0, "raise {signal} in {mode}");
    let outcome = completion
        .recv_timeout(LIMIT)
        .expect("termination must unblock existing input wait");
    assert_eq!(
        outcome.expect_err("input must report termination").kind(),
        io::ErrorKind::Interrupted,
        "{case}"
    );
    reader.join().expect("join input consumer");
    pump.stop();
    assert!(!pump.state().running, "cleanup must join the empty reader");
}

#[test]
fn termination_interrupts_running_and_stopped_input_waits() {
    for mode in [
        "running-read",
        "running-poll",
        "stopped-read",
        "stopped-poll",
    ] {
        for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            let case = format!("{mode}:{signal}");
            let mut command = Command::new(std::env::current_exe().expect("test executable"));
            command
                .args([
                    "--exact",
                    "ui::input::pump::tests::signals::termination_input_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("DIAL_TEST_INPUT_TERMINATION", &case)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = process::spawn(&mut command).expect("spawn input child");
            let exited =
                process::wait_exited(&child, Duration::from_secs(5)).expect("bounded child wait");
            if !exited {
                child.kill().expect("kill hung input child");
            }
            let output = child.wait_with_output().expect("reap input child");
            assert!(
                exited && output.status.success(),
                "{case} exited={exited}: {output:?}"
            );
        }
    }
}

//! Opt-in native power assertion check; the model is a local scripted fixture.
use super::*;

fn own_idle_assertions() -> usize {
    let output = std::process::Command::new("/usr/bin/pmset")
        .args(["-g", "assertions"])
        .output()
        .expect("inspect macOS power assertions");
    assert!(output.status.success(), "pmset failed: {output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let owner = format!("pid {}(", std::process::id());
    text.lines()
        .filter(|line| {
            line.contains(&owner)
                && line.contains("PreventUserIdleSystemSleep")
                && line.contains("Dial active agent work")
        })
        .count()
}

struct PowerUi {
    observed: bool,
    cancel_on_text: bool,
    interrupt: Arc<AtomicBool>,
}

impl Ui for PowerUi {
    fn request_policy(&mut self, _: &Value) {
        assert!(
            own_idle_assertions() > 0,
            "turn must hold an idle-sleep assertion"
        );
        self.observed = true;
    }

    fn text_delta(&mut self, _: &str) {
        if self.cancel_on_text {
            self.interrupt.store(true, Ordering::SeqCst);
        }
    }
}

#[test]
#[ignore = "requires macOS power management; run alone with --ignored --test-threads=1"]
fn native_idle_sleep_is_scoped_to_success_failure_and_cancelled_turns() {
    assert_eq!(own_idle_assertions(), 0, "run this native test alone");
    for case in 0..3 {
        let reply = if case == 1 {
            Reply::Http(400, "invalid request fixture")
        } else {
            Reply::Text("The answer is 42.")
        };
        let server = serve(vec![reply]);
        let e = env(&server.url, "auto");
        let interrupt = Arc::new(AtomicBool::new(false));
        let mut agent = agent_with(&e, None, interrupt.clone());
        assert_eq!(own_idle_assertions(), 0, "idle sessions must allow sleep");
        let mut ui = PowerUi {
            observed: false,
            cancel_on_text: case == 2,
            interrupt,
        };
        let result = agent.turn("What is six times seven?", &mut ui);
        assert!(ui.observed, "must observe an active turn in case {case}");
        match case {
            0 => assert_eq!(result.unwrap().outcome, TurnOutcome::Completed),
            1 => assert!(result.is_err(), "HTTP 400 must fail the turn"),
            2 => assert!(result.unwrap().interrupted, "turn must be cancelled"),
            _ => unreachable!(),
        }
        // Keep Agent alive: this verifies turn ownership, rather than session cleanup.
        assert_eq!(own_idle_assertions(), 0, "turn {case} leaked its assertion");
    }
}

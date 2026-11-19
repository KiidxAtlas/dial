use super::*;
use crate::agent::run::tool_exposure::default_offer_tests::with_default_agent;

struct Quiet;
impl Ui for Quiet {}

const SEMANTIC: &str = "[memory]\nbackend='semantic'\n[memory.semantic]\nembedding='off'";

fn waits(controller: &mut Controller, ends: &[PassEnd]) -> Vec<u64> {
    ends.iter()
        .map(|end| {
            controller.pass_ended(*end);
            controller.pass_interval().as_secs()
        })
        .collect()
}

#[test]
fn a_pass_that_keeps_failing_is_retried_less_often_up_to_a_cap() {
    let mut controller = Controller::default();
    assert_eq!(controller.pass_interval(), PASS_INTERVAL);
    assert_eq!(
        waits(&mut controller, &[PassEnd::Failed; 8]),
        [60, 120, 240, 480, 960, 1800, 1800, 1800]
    );
    // An hour of idle time at these waits starts 8 passes; at a fixed 30 s it started 120.
    controller.failed_passes = u32::MAX;
    controller.pass_ended(PassEnd::Failed);
    assert_eq!(controller.pass_interval(), MAX_PASS_INTERVAL);
}

#[test]
fn only_a_failure_slows_the_next_pass_and_a_finished_pass_restores_the_pace() {
    let mut controller = Controller::default();
    assert_eq!(
        waits(
            &mut controller,
            &[
                PassEnd::Yielded,
                PassEnd::Failed,
                PassEnd::Yielded,
                PassEnd::Failed,
                PassEnd::Completed,
            ]
        ),
        [30, 60, 60, 120, 30]
    );
}

#[test]
fn the_next_pass_waits_out_the_current_interval() {
    let mut controller = Controller::default();
    assert!(controller.pass_due(), "nothing has started yet");
    let Some(earlier) = Instant::now().checked_sub(Duration::from_secs(45)) else {
        return; // a clock younger than the interval cannot express the case
    };
    controller.last_start = Some(earlier);
    assert!(controller.pass_due());
    controller.pass_ended(PassEnd::Failed);
    assert!(!controller.pass_due(), "45 s is inside the doubled wait");
}

#[test]
fn a_finished_idle_pass_keeps_the_pace_and_a_turn_clears_earlier_failures() {
    with_default_agent(SEMANTIC, |agent| {
        for (role, content) in [
            (
                "user",
                "Please remember that the release branch is cut every second Tuesday.",
            ),
            (
                "assistant",
                "Noted: the release branch is cut every second Tuesday.",
            ),
        ] {
            agent
                .session
                .append(json!({"role": role, "content": content}))
                .unwrap();
        }
        let memory = agent.memory_runtime.clone().unwrap();
        let authority = memory_authority(&agent.session, 0);
        memory.bind(authority.clone());
        memory
            .observe_owned(&authority, agent.session.archived_user_assistant_messages())
            .unwrap();

        let mut ui = Quiet;
        agent.memory_maintenance_tick(&mut ui, false);
        assert!(agent.memory_controller.has_worker(), "a pass was due");
        let deadline = Instant::now() + Duration::from_secs(10);
        while agent.memory_controller.has_worker() {
            assert!(Instant::now() < deadline, "the pass did not settle");
            std::thread::sleep(Duration::from_millis(5));
            agent.memory_maintenance_tick(&mut ui, false);
        }
        assert_eq!(agent.memory_controller.failed_passes, 0);
        let nodes = agent.session.runtime().snapshots();
        assert_eq!(nodes.len(), 1, "one pass, one record");

        agent.memory_controller.failed_passes = 5;
        agent.memory_maintenance_tick(&mut ui, true);
        assert_eq!(agent.memory_controller.failed_passes, 0);
        assert_eq!(agent.memory_controller.pass_interval(), PASS_INTERVAL);
    });
}

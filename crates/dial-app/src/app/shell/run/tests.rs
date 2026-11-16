use super::*;
use crate::app::shell::testkit::shell;

#[test]
fn peer_wake_yields_to_human_input_and_pending_reconfiguration() {
    let (_home, _project, _app, mut shell) = shell();
    assert!(shell.peer_turn_available());

    shell.composer.set_draft("my next instruction");
    assert!(!shell.peer_turn_available());
    shell.composer.set_draft("");
    shell.startup = Some("queued user prompt".into());
    assert!(!shell.peer_turn_available());
    shell.startup = None;
    shell.deferred.push_back("deferred instruction".into());
    assert!(!shell.peer_turn_available());
    shell.deferred.clear();
    shell
        .held
        .push_back(Event::Paste("input already read".into()));
    assert!(!shell.peer_turn_available());
    shell.held.clear();
    shell.pending_resource_rebuild = true;
    assert!(!shell.peer_turn_available());
    shell.pending_resource_rebuild = false;
    assert!(shell.peer_turn_available());
}

#[test]
fn animation_frames_repaint_without_ending_the_wait() {
    let mut paints = 0;
    let mut steps = Vec::new();
    let ready = wait_frames(
        &mut paints,
        // A wide deadline and short polls keep the frame count independent of runner load: a
        // loaded CI host can oversleep one 10 ms frame past a tight deadline (seen as 0 paints).
        Duration::from_millis(400),
        |_| Some(Duration::from_millis(10)),
        |step| {
            steps.push(step);
            std::thread::sleep(step.min(Duration::from_millis(1)));
            Ok(false)
        },
        |paints| {
            *paints += 1;
            Ok(())
        },
    )
    .unwrap();
    assert!(!ready);
    assert!(paints >= 2, "frames painted inside one wait: {paints}");
    assert!(steps.iter().all(|s| *s <= Duration::from_millis(10)));
    assert_eq!(
        steps.len(),
        paints + 1,
        "the last poll ends the wait unpainted"
    );
}

#[test]
fn input_ends_the_wait_and_still_marks_use_one_poll() {
    let mut paints = 0;
    let mut polls = 0;
    let ready = wait_frames(
        &mut paints,
        Duration::from_secs(5),
        |_| Some(Duration::from_millis(10)),
        |_| {
            polls += 1;
            Ok(polls == 3)
        },
        |paints| {
            *paints += 1;
            Ok(())
        },
    )
    .unwrap();
    assert!(ready);
    assert_eq!((polls, paints), (3, 2));

    let mut steps = Vec::new();
    let ready = wait_frames(
        &mut paints,
        Duration::from_millis(30),
        |_| None,
        |step| {
            steps.push(step);
            Ok(false)
        },
        |_| panic!("no frames without motion"),
    )
    .unwrap();
    assert!(!ready);
    assert_eq!(steps.len(), 1);
    assert!(steps[0] <= Duration::from_millis(30));
}

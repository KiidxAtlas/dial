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

use super::*;
#[test]
fn interruption_and_session_termination_have_different_effect_scopes() {
    let session = SessionCancellation::default();
    let turn = session.turn(Arc::new(AtomicBool::new(false)));
    let other = session.turn(Arc::new(AtomicBool::new(false)));
    let mutation = turn.mutation();
    let read = turn.read();
    let inference = turn.inference();
    turn.cancel();
    assert!(mutation.interrupt.is_cancelled() && read.is_cancelled() && inference.is_cancelled());
    assert!(!mutation.commit.is_cancelled() && !other.inference().is_cancelled());
    session.cancel();
    assert!(mutation.commit.is_cancelled() && other.inference().is_cancelled());
}
#[test]
fn external_stop_flags_propagate_to_running_tools_on_the_next_owner_poll() {
    let session = SessionCancellation::default();
    let flag = Arc::new(AtomicBool::new(false));
    let turn = session.turn(flag.clone());
    let read = turn.read();
    let tool_flag = read.flag();
    flag.store(true, Ordering::SeqCst);
    turn.sync();
    assert!(tool_flag.load(Ordering::SeqCst));
}

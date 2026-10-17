use super::*;

#[test]
fn cancel_removes_job_and_sets_interrupt() {
    let mut jobs = Jobs::default();
    jobs.insert_idle("tan-1");
    let interrupt = jobs.live["tan-1"].interrupt.clone();
    assert!(jobs.cancel("tan-1"));
    assert!(interrupt.load(Ordering::SeqCst));
    assert!(!jobs.live.contains_key("tan-1"));
    assert!(!jobs.cancel("tan-1"));
}

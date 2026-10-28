use super::*;

#[test]
fn interval_is_twice_the_measured_frame_cost_within_bounds() {
    let mut pacing = FramePacing::default();
    assert_eq!(
        pacing.interval(),
        PAINT_EVERY,
        "cheap frames keep the floor"
    );
    pacing.record_frame(Duration::from_millis(30));
    assert_eq!(pacing.interval(), Duration::from_millis(60));
    pacing.record_frame(Duration::from_millis(1));
    assert_eq!(
        pacing.interval(),
        PAINT_EVERY,
        "floor holds for tiny frames"
    );
    pacing.record_frame(Duration::from_secs(1));
    assert_eq!(
        pacing.interval(),
        MAX_PAINT_EVERY,
        "expensive frames are capped"
    );
}

#[test]
fn interval_recovers_when_frames_get_cheap_again() {
    let mut pacing = FramePacing::default();
    pacing.record_frame(Duration::from_millis(150));
    assert_eq!(pacing.interval(), MAX_PAINT_EVERY);
    pacing.record_frame(Duration::from_millis(5));
    assert_eq!(pacing.interval(), PAINT_EVERY);
}

#[test]
fn sub_threshold_phases_are_not_stalls() {
    let mut pacing = FramePacing::default();
    pacing.record(Phase::Paint, STALL_AFTER - Duration::from_millis(1));
    assert_eq!(pacing.stalls().count(), 0);
}

#[test]
fn stalls_keep_their_phase_and_the_log_is_bounded() {
    let mut pacing = FramePacing::default();
    pacing.record(Phase::Input, STALL_AFTER);
    for _ in 0..STALL_CAPACITY {
        pacing.record(Phase::Paint, Duration::from_millis(250));
    }
    let stalls: Vec<Stall> = pacing.stalls().collect();
    assert_eq!(stalls.len(), STALL_CAPACITY);
    assert!(
        stalls.iter().all(|s| s.phase == Phase::Paint),
        "oldest dropped first"
    );
}

#[test]
fn frame_measures_count_frames_and_keep_the_worst() {
    let mut pacing = FramePacing::default();
    pacing.record_frame(Duration::from_millis(12));
    pacing.record_frame(Duration::from_millis(40));
    pacing.record_frame(Duration::from_millis(8));
    assert_eq!(pacing.frames, 3);
    assert_eq!(pacing.worst_frame, Duration::from_millis(40));
}

use super::*;

const OWNER: Owner = Owner {
    domain: 0xfeed_beef,
    pid: 4242,
    start: 777,
};

#[test]
fn owner_name_round_trips_at_integer_limits() {
    let owner = Owner {
        domain: u64::MAX,
        pid: u32::MAX,
        start: u64::MAX,
    };
    let pointer = format!("{} pointer", owner.master_name(u16::MAX));
    assert_eq!(Owner::from_pointer_name(&pointer), Some(owner));
    assert!(format!("{} uinput keyboard", owner.master_name(u16::MAX)).len() < 80);
    assert_eq!(Owner::from_pointer_name("DIAL MPX 4242.1 pointer"), None);
}

#[test]
fn only_provably_dead_owners_are_stale() {
    let peer = |pid, start| Owner {
        pid,
        start,
        ..OWNER
    };
    let gone = |_| Err(io::Error::from(io::ErrorKind::NotFound));
    let denied = |_| Err(io::Error::from(io::ErrorKind::PermissionDenied));
    assert!(peer(99, 5).is_stale_for(&OWNER, gone));
    assert!(peer(99, 5).is_stale_for(&OWNER, |_| Ok(6)), "reused pid");
    assert!(
        !peer(99, 5).is_stale_for(&OWNER, |_| Ok(5)),
        "live peer session"
    );
    assert!(!peer(99, 5).is_stale_for(&OWNER, denied), "unverifiable");
    assert!(!OWNER.is_stale_for(&OWNER, gone), "this process");
    assert!(
        peer(OWNER.pid, 1).is_stale_for(&OWNER, gone),
        "earlier incarnation of our pid"
    );
    let foreign = Owner {
        domain: 1,
        ..peer(99, 5)
    };
    assert!(
        !foreign.is_stale_for(&OWNER, gone),
        "other machine or namespace"
    );
}

#[test]
fn start_ticks_survives_parentheses_in_the_command_name() {
    let stat = "12 (a) b (c)) S 1 12 12 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 98765 0";
    assert_eq!(start_ticks(stat), Some(98765));
}

#[test]
fn drag_steps_are_bounded_and_land_exactly_on_the_end() {
    let steps = relative_steps(&[(10, 10), (10, 10), (60, 22), (58, 22)]);
    assert!(
        steps
            .iter()
            .all(|&(dx, dy)| f64::from(dx).hypot(f64::from(dy)) <= DRAG_STEP_MAX_PX)
    );
    assert!(steps.iter().all(|&step| step != (0, 0)));
    let end = steps
        .iter()
        .fold((10, 10), |(x, y), &(dx, dy)| (x + dx, y + dy));
    assert_eq!(end, (58, 22));
}

#[test]
fn wheel_detents_follow_x_button_directions() {
    // X: dy > 0 is button 5 (down) = REL_WHEEL -1; dx > 0 is button 7
    // (right) = REL_HWHEEL +1.
    assert_eq!(wheel_detents(0.0, 2.0), vec![(false, -1), (false, -1)]);
    assert_eq!(wheel_detents(0.0, -1.0), vec![(false, 1)]);
    assert_eq!(wheel_detents(1.0, 0.0), vec![(true, 1)]);
    assert_eq!(wheel_detents(-1.0, 0.0), vec![(true, -1)]);
    assert!(wheel_detents(0.2, -0.4).is_empty());
}

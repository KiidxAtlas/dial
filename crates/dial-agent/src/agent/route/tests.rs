use super::*;

fn on_fallback_until(until: Instant) -> ActiveRoute {
    ActiveRoute {
        tried: 1,
        fallback: Some(Fallback {
            until,
            primary_cap: Some(8192),
        }),
    }
}

#[test]
fn fallback_is_retained_while_cooling_and_restored_once_elapsed() {
    let now = Instant::now();
    let route = on_fallback_until(now + Duration::from_secs(300));
    assert!(route.on_fallback());
    assert!(route.cooling_at(now));
    assert!(!route.restore_due_at(now));
    // The cooldown boundary itself already allows the primary back.
    let route = on_fallback_until(now);
    assert!(!route.cooling_at(now));
    assert!(route.restore_due_at(now));
}

#[test]
fn primary_route_never_restores_and_reset_forgets_the_episode() {
    let now = Instant::now();
    let mut route = ActiveRoute::default();
    assert!(!route.on_fallback());
    assert!(!route.restore_due_at(now));
    route = on_fallback_until(now);
    assert!(route.restore_due_at(now));
    route.reset();
    assert!(!route.on_fallback());
    assert_eq!(route.tried, 0);
}

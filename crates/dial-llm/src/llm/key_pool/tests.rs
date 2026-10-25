use super::*;

fn pool(names: &[&str]) -> KeyPool {
    KeyPool::new(names.iter().map(|name| (*name).to_owned())).unwrap()
}

const ALWAYS: fn(&str) -> bool = |_| true;

#[test]
fn fewer_than_two_distinct_variables_is_not_a_pool() {
    assert!(KeyPool::new(["A".to_owned()]).is_none());
    assert!(KeyPool::new(["A".to_owned(), " A ".to_owned(), "".to_owned()]).is_none());
    assert_eq!(pool(&["A", "B", "A"]).envs, ["A", "B"]);
}

#[test]
fn rotation_blocks_the_failed_key_and_sticks_to_the_working_one() {
    let blocks = Blocks::default();
    let now = Instant::now();
    let mut keys = pool(&["A", "B", "C"]);
    assert_eq!(keys.active(), "A");
    assert_eq!(
        keys.rotate(&blocks, "ep", now, Duration::from_secs(60), ALWAYS),
        Some("B")
    );
    assert!(blocks.is_blocked("ep", "A", now));
    // Affinity: B keeps serving; the elapsed block on A does not pull the pool back.
    let later = now + Duration::from_secs(61);
    assert!(!blocks.is_blocked("ep", "A", later));
    keys.settle(&blocks, "ep", later, ALWAYS);
    assert_eq!(keys.active(), "B");
    // B fails: A is usable again, C comes first in order after B.
    assert_eq!(
        keys.rotate(&blocks, "ep", later, Duration::from_secs(10), ALWAYS),
        Some("C")
    );
}

#[test]
fn rotation_reports_exhaustion_and_skips_blocked_or_unset_keys() {
    let blocks = Blocks::default();
    let now = Instant::now();
    let mut keys = pool(&["A", "B", "C"]);
    blocks.block("ep", "B", now + Duration::from_secs(30));
    // B is blocked and C has no value: nothing to rotate to, and A stays blocked.
    assert_eq!(
        keys.rotate(&blocks, "ep", now, Duration::from_secs(60), |env| env
            != "C"),
        None
    );
    assert!(blocks.is_blocked("ep", "A", now));
    assert_eq!(keys.active(), "A");
    // Another request already blocked the active key: settle moves off it before dispatch.
    let mut other = pool(&["A", "B", "C"]);
    other.settle(&blocks, "ep", now, ALWAYS);
    assert_eq!(other.active(), "C");
}

#[test]
fn blocks_are_scoped_per_endpoint_and_expire() {
    let blocks = Blocks::default();
    let now = Instant::now();
    blocks.block("one", "K", now + Duration::from_secs(5));
    assert!(blocks.is_blocked("one", "K", now));
    assert!(!blocks.is_blocked("two", "K", now));
    assert!(!blocks.is_blocked("one", "K", now + Duration::from_secs(5)));
}

#[test]
fn cooldown_follows_retry_after_or_reset_and_defaults_to_a_minute() {
    let now = SystemTime::now();
    let limited = |retry_after| ErrorClass::RateLimited { retry_after };
    assert_eq!(cooldown(&limited(None), None, now), Some(DEFAULT_COOLDOWN));
    assert_eq!(
        cooldown(&limited(Some(Duration::from_secs(7))), None, now),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        cooldown(&limited(None), Some(Duration::from_secs(9)), now),
        Some(Duration::from_secs(9))
    );
    let quota = ErrorClass::QuotaExhausted {
        reset_at: Some(now + Duration::from_secs(600)),
    };
    let wait = cooldown(&quota, None, now).unwrap();
    assert!((599..=600).contains(&wait.as_secs()), "{wait:?}");
    assert_eq!(
        cooldown(&ErrorClass::QuotaExhausted { reset_at: None }, None, now),
        Some(DEFAULT_COOLDOWN)
    );
    let far = ErrorClass::QuotaExhausted {
        reset_at: Some(now + Duration::from_secs(86_400 * 30)),
    };
    assert_eq!(cooldown(&far, None, now), Some(MAX_COOLDOWN));
    assert_eq!(cooldown(&ErrorClass::Transient, None, now), None);
}

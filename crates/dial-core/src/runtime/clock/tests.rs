use super::*;
use std::time::{Duration, Instant};

#[test]
fn instant_ago_is_in_the_past_and_never_panics() {
    let before = Instant::now();
    let ago = instant_ago(Duration::from_millis(5));
    assert!(ago <= before);
    assert!(ago.elapsed() >= Duration::from_millis(5));
    // Larger than any clock's distance from its origin: clamps instead of panicking.
    assert!(instant_ago(Duration::MAX) >= before);
}

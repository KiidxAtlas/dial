use super::*;
use serde_json::json;
use std::time::Duration;

fn pricing() -> TokenPricing {
    TokenPricing {
        prompt: 3.0 / 1_000_000.0,
        completion: 15.0 / 1_000_000.0,
        cache_read: Some(0.3 / 1_000_000.0),
        cache_write: Some(3.75 / 1_000_000.0),
    }
}

fn chat() -> Chat {
    Chat {
        messages: vec![json!({"role": "user", "content": "hi"})],
        tools: vec![],
        temperature: 0.0,
        max_tokens: 4096,
        seed: None,
    }
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9
}

#[test]
fn short_tier_miss_is_the_write_premium_over_a_read() {
    let econ = economics(pricing(), 1_000_000, Tier::Short).unwrap();
    assert!(close(econ.replay_usd, 0.3 + 15.0 / 1_000_000.0 * 1.0));
    assert!(close(econ.miss_usd, 3.45));
}

#[test]
fn long_tier_miss_uses_the_one_hour_write_rate() {
    let econ = economics(pricing(), 1_000_000, Tier::Long).unwrap();
    assert!(close(econ.miss_usd, 5.7));
}

#[test]
fn economics_refuse_a_card_without_cache_rates() {
    let mut card = pricing();
    card.cache_read = None;
    assert!(economics(card, 1000, Tier::Short).is_none());
}

#[test]
fn small_prefixes_on_idle_do_not_clear_the_savings_floor() {
    let econ = economics(pricing(), 1000, Tier::Short).unwrap();
    assert!(worth_replaying(econ, Mode::Streaming));
    assert!(!worth_replaying(econ, Mode::Idle));
}

#[test]
fn off_mode_never_arms() {
    let mut warmer = Warmer::default();
    let now = Instant::now();
    let result = warmer.arm(
        &chat(),
        1_000_000,
        Some(pricing()),
        Tier::Short,
        Mode::Off,
        now,
    );
    assert_eq!(result, Err(Stop::Mode));
    assert_eq!(warmer.due(), None);
}

#[test]
fn unpriced_requests_stop_instead_of_guessing() {
    let mut warmer = Warmer::default();
    let result = warmer.arm(
        &chat(),
        1000,
        None,
        Tier::Short,
        Mode::Streaming,
        Instant::now(),
    );
    assert_eq!(result, Err(Stop::Unpriced));
}

#[test]
fn replay_waits_for_the_refresh_point_and_caps_output_at_one_token() {
    let mut warmer = Warmer::default();
    let start = Instant::now();
    warmer
        .arm(
            &chat(),
            1_000_000,
            Some(pricing()),
            Tier::Short,
            Mode::Streaming,
            start,
        )
        .unwrap();
    // Five-minute lifetime, refreshed 60 seconds early.
    assert_eq!(warmer.due(), Some(start + Duration::from_secs(240)));
    assert!(
        warmer
            .replay(start + Duration::from_secs(239))
            .unwrap()
            .is_none()
    );
    let replay = warmer
        .replay(start + Duration::from_secs(240))
        .unwrap()
        .expect("replay due");
    assert_eq!(replay.max_tokens, 1);
    assert_eq!(replay.messages, chat().messages);
    assert_eq!(warmer.due(), Some(start + Duration::from_secs(480)));
}

#[test]
fn warming_stops_once_the_window_after_the_last_turn_elapses() {
    let mut warmer = Warmer::default();
    let start = Instant::now();
    warmer
        .arm(
            &chat(),
            1_000_000,
            Some(pricing()),
            Tier::Short,
            Mode::Streaming,
            start,
        )
        .unwrap();
    let late = start + STREAMING_WINDOW;
    assert!(matches!(warmer.replay(late), Err(Stop::WindowElapsed)));
    assert_eq!(warmer.due(), None);
    assert!(matches!(warmer.replay(late), Ok(None)));
}

#[test]
fn a_new_real_turn_replaces_the_armed_request() {
    let mut warmer = Warmer::default();
    let start = Instant::now();
    warmer
        .arm(
            &chat(),
            1_000_000,
            Some(pricing()),
            Tier::Short,
            Mode::Streaming,
            start,
        )
        .unwrap();
    let later = start + Duration::from_secs(100);
    warmer
        .arm(
            &chat(),
            1_000_000,
            Some(pricing()),
            Tier::Short,
            Mode::Streaming,
            later,
        )
        .unwrap();
    assert_eq!(warmer.due(), Some(later + Duration::from_secs(240)));
}

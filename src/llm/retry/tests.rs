use super::*;

fn at(headers: &[(&str, &str)], now: SystemTime) -> ServerHints {
    hints(
        |name| headers.iter().find(|(n, _)| *n == name).map(|(_, v)| *v),
        now,
    )
}

fn policy() -> Policy {
    Policy::new(
        &crate::config::types::RetryConfig::default(),
        Duration::from_millis(10),
    )
}

#[test]
fn explicit_delay_headers_are_parsed() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    for (header, value, expected) in [
        ("retry-after", "4", 4.0),
        ("retry-after-ms", "250", 0.25),
        ("x-ratelimit-reset-ms", "1500", 1.5),
        ("x-ratelimit-reset", "7", 7.0),
        ("x-ratelimit-reset", "1800000030", 30.0),
        ("x-ratelimit-reset-ms", "1800000002000", 2.0),
    ] {
        assert_eq!(
            at(&[(header, value)], now).delay.unwrap().as_secs_f64(),
            expected,
            "{header}"
        );
    }
    assert_eq!(
        at(&[("retry-after", "Thu, 01 Jan 1970 00:00:01 GMT")], now).delay,
        None
    );
    assert_eq!(at(&[("retry-after", "0")], now).delay, None);
    assert_eq!(
        at(&[("retry-after", "2"), ("retry-after-ms", "5000")], now).delay,
        Some(Duration::from_secs(5))
    );
    assert_eq!(
        reset_date("1970-01-01T00:00:01Z"),
        Some(UNIX_EPOCH + Duration::from_secs(1))
    );
}

#[test]
fn window_reset_headers_never_become_the_retry_delay() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let hints = at(
        &[
            ("retry-after", "1"),
            ("x-ratelimit-reset-tokens", "6m0s"),
            ("anthropic-ratelimit-tokens-reset", "2027-01-15T08:00:00Z"),
        ],
        now,
    );
    assert_eq!(hints.delay, Some(Duration::from_secs(1)));
    assert!(
        hints
            .reset_at
            .is_some_and(|reset| reset >= now + Duration::from_secs(360))
    );
    let only_window = at(&[("x-ratelimit-reset-requests", "1m2s")], now);
    assert_eq!(only_window.delay, None);
    assert_eq!(only_window.reset_at, Some(now + Duration::from_secs(62)));
}

#[test]
fn overloaded_with_short_retry_after_and_long_window_reset_retries() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let hints = at(
        &[
            ("retry-after", "1"),
            ("anthropic-ratelimit-unified-reset", "1800003600"),
        ],
        now,
    );
    let class = ErrorClass::RateLimited {
        retry_after: hints.delay,
    };
    match decide(&class, hints.delay, 1, &policy()) {
        Decision::Retry { delay, .. } => {
            assert!(delay >= Duration::from_secs(1) && delay < Duration::from_secs(2))
        }
        other => panic!("529 with retry-after:1 must retry, got {other:?}"),
    }
}

#[test]
fn quota_exhaustion_fails_fast_unless_waiting_is_opted_in() {
    let reset = Some(SystemTime::now() + Duration::from_secs(30));
    let class = ErrorClass::QuotaExhausted { reset_at: reset };
    assert!(matches!(
        decide(&class, None, 1, &policy()),
        Decision::Stop(None)
    ));
    let mut waiting = policy();
    waiting.wait_for_quota_reset = true;
    assert!(
        matches!(decide(&class, None, 1, &waiting), Decision::Retry { delay, .. } if delay >= Duration::from_secs(29))
    );
    waiting.max_delay = Duration::from_secs(10);
    assert!(matches!(
        decide(&class, None, 1, &waiting),
        Decision::Stop(Some(_))
    ));
}

#[test]
fn attempts_and_delay_ceiling_bound_retries() {
    let policy = policy();
    assert!(matches!(
        decide(&ErrorClass::Transient, None, 1, &policy),
        Decision::Retry { .. }
    ));
    assert!(matches!(
        decide(&ErrorClass::Transient, None, policy.max_attempts, &policy),
        Decision::Stop(Some(_))
    ));
    let long = Some(Duration::from_secs(301));
    assert!(matches!(
        decide(
            &ErrorClass::RateLimited { retry_after: long },
            long,
            1,
            &policy
        ),
        Decision::Stop(Some(_))
    ));
    assert!(matches!(
        decide(&ErrorClass::Fatal, None, 1, &policy),
        Decision::Stop(None)
    ));
    assert!(matches!(
        decide(&ErrorClass::ContextOverflow, None, 1, &policy),
        Decision::Stop(None)
    ));
    assert!(matches!(
        decide(&ErrorClass::Auth, None, 1, &policy),
        Decision::Stop(None)
    ));
}

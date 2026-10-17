use super::*;
use std::collections::HashMap;

fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

fn query() -> Query<'static> {
    Query {
        text: "rust async",
        limit: 3,
        recency: None,
        today: 19_723,
    }
}

#[test]
fn auto_selects_in_order_and_always_ends_with_keyless_fallback() {
    let none = env_from(&[]);
    assert_eq!(chain("", &none).unwrap(), vec![Provider::DuckDuckGo]);
    assert_eq!(chain("auto", &none).unwrap(), vec![Provider::DuckDuckGo]);

    let some = env_from(&[
        ("BRAVE_API_KEY", "b"),
        ("SEARXNG_URL", "https://search.example"),
        ("GOOGLE_API_KEY", "g"),
    ]);
    // Google needs its CSE id too, so it is skipped.
    assert_eq!(
        chain("auto", &some).unwrap(),
        vec![Provider::Brave, Provider::Searxng, Provider::DuckDuckGo]
    );

    let all = env_from(&[
        ("EXA_API_KEY", "e"),
        ("BRAVE_API_KEY", "b"),
        ("GOOGLE_API_KEY", "g"),
        ("GOOGLE_CSE_ID", "cx"),
    ]);
    let order = chain("auto", &all).unwrap();
    assert_eq!(order.first(), Some(&Provider::Exa));
    assert_eq!(order.last(), Some(&Provider::DuckDuckGo));
}

#[test]
fn explicit_provider_is_used_alone_and_names_the_missing_variable() {
    let env = env_from(&[("TAVILY_API_KEY", "t")]);
    assert_eq!(chain("tavily", &env).unwrap(), vec![Provider::Tavily]);
    assert_eq!(
        chain("duckduckgo", &env).unwrap(),
        vec![Provider::DuckDuckGo]
    );

    let missing = chain("brave", &env_from(&[])).unwrap_err();
    assert!(missing.contains("brave") && missing.contains("BRAVE_API_KEY"));
    let google = chain("google", &env_from(&[("GOOGLE_API_KEY", "g")])).unwrap_err();
    assert!(google.contains("GOOGLE_CSE_ID") && !google.contains("GOOGLE_API_KEY"));
    assert!(
        chain("nope", &env)
            .unwrap_err()
            .contains("unknown search provider")
    );
}

#[test]
fn keys_are_checked_for_header_safety_without_echoing_them() {
    let env = env_from(&[("BRAVE_API_KEY", "bad key\u{7f}")]);
    let error = build(Provider::Brave, &query(), &env).unwrap_err();
    assert!(error.contains("BRAVE_API_KEY") && !error.contains("bad key"));
    assert!(
        build(Provider::Brave, &query(), &env_from(&[]))
            .unwrap_err()
            .contains("not set")
    );
}

#[test]
fn bing_and_searxng_endpoints_must_be_https_or_absolute() {
    let plain = env_from(&[(
        "BING_SEARCH_ENDPOINT",
        "http://insecure.example/v7.0/search",
    )]);
    assert!(bing_endpoint(&plain).is_err());
    let azure = env_from(&[(
        "BING_SEARCH_ENDPOINT",
        "https://res.cognitiveservices.azure.com/bing/v7.0/search/",
    )]);
    assert_eq!(
        bing_endpoint(&azure).unwrap(),
        "https://res.cognitiveservices.azure.com/bing/v7.0/search"
    );
    assert_eq!(bing_endpoint(&env_from(&[])).unwrap(), json_apis::BING_URL);
    assert!(searxng_endpoint(&env_from(&[("SEARXNG_URL", "search.example")])).is_err());
    assert_eq!(
        searxng_endpoint(&env_from(&[("SEARXNG_URL", "https://s.example/")])).unwrap(),
        "https://s.example"
    );
}

#[test]
fn duckduckgo_build_encodes_the_query() {
    let request = build(Provider::DuckDuckGo, &query(), &env_from(&[])).unwrap();
    assert_eq!(request.method(), "GET");
    assert_eq!(
        request.url,
        "https://html.duckduckgo.com/html/?q=rust%20async"
    );
    assert!(request.headers.is_empty() && request.body.is_none());
}

#[test]
fn duckduckgo_build_maps_recency_to_its_date_filter() {
    let cases = [
        (Recency::Day, "d"),
        (Recency::Week, "w"),
        (Recency::Month, "m"),
        (Recency::Year, "y"),
    ];
    for (recency, code) in cases {
        let windowed = Query {
            recency: Some(recency),
            ..query()
        };
        let request = build(Provider::DuckDuckGo, &windowed, &env_from(&[])).unwrap();
        assert_eq!(
            request.url,
            format!("https://html.duckduckgo.com/html/?q=rust%20async&df={code}")
        );
    }
}

#[test]
fn recency_note_flags_only_providers_that_do_not_honour_the_window() {
    let jina = Provider::Jina.recency_note(Recency::Day).unwrap();
    assert!(jina.contains("jina") && jina.contains("past day"), "{jina}");
    let searxng = Provider::Searxng.recency_note(Recency::Week).unwrap();
    assert!(searxng.contains("past month"), "{searxng}");
    assert_eq!(Provider::Searxng.recency_note(Recency::Day), None);
    for provider in ALL
        .into_iter()
        .filter(|provider| !matches!(provider, Provider::Jina | Provider::Searxng))
    {
        for recency in [Recency::Day, Recency::Week, Recency::Month, Recency::Year] {
            assert_eq!(provider.recency_note(recency), None, "{provider:?}");
        }
    }
}

#[test]
fn recency_names_and_windows() {
    assert_eq!(Recency::parse("week"), Some(Recency::Week));
    assert_eq!(Recency::parse("hour"), None);
    assert_eq!(Recency::Year.name(), "year");
    assert_eq!(Recency::Month.days(), 31);
}

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
    let keyless = vec![
        Provider::DuckDuckGo,
        Provider::Startpage,
        Provider::Mojeek,
        Provider::Ecosia,
    ];
    assert_eq!(chain("", &none).unwrap(), keyless);
    assert_eq!(chain("auto", &none).unwrap(), keyless);

    let some = env_from(&[
        ("BRAVE_API_KEY", "b"),
        ("SEARXNG_URL", "https://search.example"),
        ("GOOGLE_API_KEY", "g"),
    ]);
    // Google needs its CSE id too, so it is skipped.
    assert_eq!(
        chain("auto", &some).unwrap(),
        vec![
            Provider::Brave,
            Provider::Searxng,
            Provider::DuckDuckGo,
            Provider::Startpage,
            Provider::Mojeek,
            Provider::Ecosia
        ]
    );

    let all = env_from(&[
        ("EXA_API_KEY", "e"),
        ("BRAVE_API_KEY", "b"),
        ("GOOGLE_API_KEY", "g"),
        ("GOOGLE_CSE_ID", "cx"),
    ]);
    let order = chain("auto", &all).unwrap();
    assert_eq!(order.first(), Some(&Provider::Exa));
    assert_eq!(order.last(), Some(&Provider::Ecosia));
}

#[test]
fn new_keyed_providers_are_skipped_by_auto_without_a_key_and_ordered_with_one() {
    let none = chain("auto", &env_from(&[])).unwrap();
    for provider in [
        Provider::Firecrawl,
        Provider::Parallel,
        Provider::Zai,
        Provider::Xai,
    ] {
        assert!(!none.contains(&provider), "{provider:?}");
    }
    let keyed = env_from(&[
        ("XAI_API_KEY", "x"),
        ("ZAI_API_KEY", "z"),
        ("PARALLEL_API_KEY", "p"),
        ("FIRECRAWL_API_KEY", "f"),
        ("SERPER_API_KEY", "s"),
    ]);
    let order = chain("auto", &keyed).unwrap();
    assert_eq!(
        &order[..5],
        &[
            Provider::Serper,
            Provider::Firecrawl,
            Provider::Parallel,
            Provider::Zai,
            Provider::Xai
        ]
    );
    // A blank key counts as unset.
    assert!(
        !chain("auto", &env_from(&[("XAI_API_KEY", "  ")]))
            .unwrap()
            .contains(&Provider::Xai)
    );
}

#[test]
fn new_providers_are_selectable_by_name() {
    let env = env_from(&[
        ("FIRECRAWL_API_KEY", "f"),
        ("PARALLEL_API_KEY", "p"),
        ("ZAI_API_KEY", "z"),
        ("XAI_API_KEY", "x"),
    ]);
    for (name, provider) in [
        ("firecrawl", Provider::Firecrawl),
        ("parallel", Provider::Parallel),
        ("zai", Provider::Zai),
        ("xai", Provider::Xai),
        ("startpage", Provider::Startpage),
        ("mojeek", Provider::Mojeek),
        ("ecosia", Provider::Ecosia),
    ] {
        assert!(names().contains(&name), "{name}");
        assert_eq!(chain(name, &env).unwrap(), vec![provider]);
        assert!(build(provider, &query(), &env).is_ok(), "{name}");
    }
    let missing = chain("zai", &env_from(&[])).unwrap_err();
    assert!(missing.contains("ZAI_API_KEY"), "{missing}");
    assert!(
        build(Provider::Xai, &query(), &env_from(&[]))
            .unwrap_err()
            .contains("XAI_API_KEY")
    );
}

#[test]
fn parse_dispatch_uses_the_scraper_parsers_and_rejects_blocked_pages() {
    let response = |body: &str, media: &str| Response {
        url: String::new(),
        status: 200,
        media_type: Some(media.into()),
        charset: None,
        body: body.as_bytes().to_vec(),
        truncated: false,
    };
    let page = include_str!("scrapers/fixtures/mojeek.html");
    let parsed = parse(Provider::Mojeek, &response(page, "text/html")).unwrap();
    assert_eq!(parsed.results.len(), 3);
    let wall = include_str!("scrapers/fixtures/mojeek_captcha.html");
    assert!(parse(Provider::Mojeek, &response(wall, "text/html")).is_err());
    let json = parse(Provider::Mojeek, &response("{}", "application/json")).unwrap_err();
    assert!(json.contains("non-HTML"), "{json}");
    let zai = response(
        r#"{"search_result":[{"title":"t","link":"https://a.example/"}]}"#,
        "application/json",
    );
    assert_eq!(parse(Provider::Zai, &zai).unwrap().results.len(), 1);
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
    for provider in [Provider::Mojeek, Provider::Ecosia] {
        let note = provider.recency_note(Recency::Day).unwrap();
        assert!(
            note.contains(provider.name()) && note.contains("past day"),
            "{note}"
        );
    }
    let xai = Provider::Xai.recency_note(Recency::Month).unwrap();
    assert!(xai.contains("xai") && xai.contains("past month"), "{xai}");
    for provider in ALL.into_iter().filter(|provider| {
        !matches!(
            provider,
            Provider::Jina
                | Provider::Searxng
                | Provider::Mojeek
                | Provider::Ecosia
                | Provider::Xai
        )
    }) {
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

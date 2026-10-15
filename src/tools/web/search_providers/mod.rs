//! Search backends for `web_search`: keyed REST APIs, a self-hosted SearXNG, and the keyless
//! DuckDuckGo HTML fallback.
//!
//! Every provider is a pure request builder plus a pure response parser. [`search`] performs the
//! single HTTP exchange per attempt through the guarded [`Client`]. API keys are read from
//! environment variables, travel only in headers (Google CSE: its query string) to the provider's
//! own https origin, and never appear in results, errors or logs.

mod json_apis;
mod perplexity;
mod searxng;

use super::content;
use super::fetch::{Client, Response, percent_encode};
use super::search::{self, SearchResult};
use serde_json::Value;

const ACCEPT: &str = "application/json,text/html;q=0.8,*/*;q=0.5";

/// Environment lookup, injected so selection and key handling are testable without touching the process
/// environment.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Brave,
    Tavily,
    Exa,
    Serper,
    Kagi,
    Jina,
    Perplexity,
    Google,
    Bing,
    Searxng,
    DuckDuckGo,
}

const ALL: [Provider; 11] = [
    Provider::Brave,
    Provider::Tavily,
    Provider::Exa,
    Provider::Serper,
    Provider::Kagi,
    Provider::Jina,
    Provider::Perplexity,
    Provider::Google,
    Provider::Bing,
    Provider::Searxng,
    Provider::DuckDuckGo,
];

/// Order used by `auto`: the first provider whose environment is configured wins; DuckDuckGo is the
/// keyless last resort and is always available.
const AUTO_ORDER: [Provider; 11] = [
    Provider::Exa,
    Provider::Brave,
    Provider::Tavily,
    Provider::Perplexity,
    Provider::Kagi,
    Provider::Jina,
    Provider::Serper,
    Provider::Bing,
    Provider::Google,
    Provider::Searxng,
    Provider::DuckDuckGo,
];

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Brave => "brave",
            Provider::Tavily => "tavily",
            Provider::Exa => "exa",
            Provider::Serper => "serper",
            Provider::Kagi => "kagi",
            Provider::Jina => "jina",
            Provider::Perplexity => "perplexity",
            Provider::Google => "google",
            Provider::Bing => "bing",
            Provider::Searxng => "searxng",
            Provider::DuckDuckGo => "duckduckgo",
        }
    }

    /// Variables that must all be set for `auto` to select this provider.
    fn required_env(self) -> &'static [&'static str] {
        match self {
            Provider::Brave => &["BRAVE_API_KEY"],
            Provider::Tavily => &["TAVILY_API_KEY"],
            Provider::Exa => &["EXA_API_KEY"],
            Provider::Serper => &["SERPER_API_KEY"],
            Provider::Kagi => &["KAGI_API_KEY"],
            Provider::Jina => &["JINA_API_KEY"],
            Provider::Perplexity => &["PERPLEXITY_API_KEY"],
            Provider::Google => &["GOOGLE_API_KEY", "GOOGLE_CSE_ID"],
            Provider::Bing => &["BING_API_KEY"],
            Provider::Searxng => &["SEARXNG_URL"],
            Provider::DuckDuckGo => &[],
        }
    }

    fn from_name(name: &str) -> Option<Provider> {
        ALL.into_iter().find(|provider| provider.name() == name)
    }

    /// How this provider deviates from the requested `recency` window, if it does, so the caller can
    /// say so instead of presenting unfiltered results as recent ones.
    pub fn recency_note(self, recency: Recency) -> Option<String> {
        match (self, recency) {
            (Provider::Jina, _) => Some(format!(
                "jina does not support the recency filter, so results are not limited to the past {}",
                recency.name()
            )),
            (Provider::Searxng, Recency::Week) => {
                Some("searxng has no week window, so results cover the past month".into())
            }
            _ => None,
        }
    }
}

/// Names accepted by `web.search_provider` and the `provider` tool argument, `auto` first.
pub fn names() -> Vec<&'static str> {
    std::iter::once("auto")
        .chain(ALL.into_iter().map(Provider::name))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl Recency {
    pub fn parse(text: &str) -> Option<Recency> {
        match text {
            "day" => Some(Recency::Day),
            "week" => Some(Recency::Week),
            "month" => Some(Recency::Month),
            "year" => Some(Recency::Year),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Recency::Day => "day",
            Recency::Week => "week",
            Recency::Month => "month",
            Recency::Year => "year",
        }
    }

    /// Length of the window in days; a month is 31 days so the window never undershoots.
    fn days(self) -> i64 {
        match self {
            Recency::Day => 1,
            Recency::Week => 7,
            Recency::Month => 31,
            Recency::Year => 365,
        }
    }
}

/// One validated search request. `today` is days since the Unix epoch, passed in so builders stay pure.
#[derive(Debug, Clone, Copy)]
pub struct Query<'a> {
    pub text: &'a str,
    pub limit: usize,
    pub recency: Option<Recency>,
    pub today: i64,
}

/// A provider's HTTP exchange. A body makes it a POST with a JSON document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
}

impl HttpRequest {
    fn get(url: String, headers: Vec<(&'static str, String)>) -> Self {
        Self {
            url,
            headers,
            body: None,
        }
    }

    fn post(url: &str, headers: Vec<(&'static str, String)>, body: Value) -> Self {
        Self {
            url: url.to_owned(),
            headers,
            body: Some(body.to_string()),
        }
    }

    pub fn method(&self) -> &'static str {
        if self.body.is_some() { "POST" } else { "GET" }
    }
}

/// A parsed provider reply: an optional synthesized answer and the cited results.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub answer: Option<String>,
    pub results: Vec<SearchResult>,
}

/// The provider that answered a search, with what it returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub provider: Provider,
    pub answer: Option<String>,
    pub results: Vec<SearchResult>,
}

/// Providers to try, in order, for a setting (`web.search_provider` or a per-call override). An empty
/// setting or `auto` selects every provider whose environment is configured, in [`AUTO_ORDER`]. A named
/// provider is used alone and fails clearly when its environment is incomplete.
pub fn chain(setting: &str, env: Env) -> Result<Vec<Provider>, String> {
    match setting.trim() {
        "" | "auto" => Ok(AUTO_ORDER
            .into_iter()
            .filter(|provider| {
                provider
                    .required_env()
                    .iter()
                    .all(|name| configured(env, name))
            })
            .collect()),
        name => {
            let provider = Provider::from_name(name).ok_or_else(|| {
                format!(
                    "unknown search provider {name:?}; expected one of: {}",
                    names().join(", ")
                )
            })?;
            let missing: Vec<&str> = provider
                .required_env()
                .iter()
                .copied()
                .filter(|variable| !configured(env, variable))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "search provider {name} needs {} set in the environment",
                    missing.join(" and ")
                ));
            }
            Ok(vec![provider])
        }
    }
}

/// Run the chain: the first provider that answers wins. Failures fall through to the next provider, so
/// with an explicit single provider the failure is reported as is.
pub fn search(
    client: &Client,
    chain: &[Provider],
    query: &Query,
    env: Env,
) -> Result<Found, String> {
    let mut failures = Vec::new();
    for &provider in chain {
        match attempt(client, provider, query, env) {
            Ok(mut parsed) => {
                parsed.results.retain(|result| !result.url.is_empty());
                parsed.results.truncate(query.limit);
                return Ok(Found {
                    provider,
                    answer: parsed.answer,
                    results: parsed.results,
                });
            }
            Err(message) => failures.push(format!("{}: {message}", provider.name())),
        }
    }
    if failures.is_empty() {
        return Err("no search provider is available".into());
    }
    Err(failures.join("; "))
}

fn attempt(client: &Client, provider: Provider, query: &Query, env: Env) -> Result<Parsed, String> {
    let request = build(provider, query, env)?;
    let headers: Vec<(&str, &str)> = request
        .headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let post = match request.method() {
        "POST" => request
            .body
            .as_deref()
            .map(|body| ("application/json", body.as_bytes())),
        _ => None,
    };
    let response = client.send(&request.url, ACCEPT, &headers, post)?;
    if !(200..300).contains(&response.status) {
        return Err(format!("HTTP {}", response.status));
    }
    parse(provider, &response)
}

/// Pure: the exchange this provider needs for `query`, with keys from `env`.
pub fn build(provider: Provider, query: &Query, env: Env) -> Result<HttpRequest, String> {
    Ok(match provider {
        Provider::Brave => json_apis::brave(query, &secret(env, "BRAVE_API_KEY")?),
        Provider::Tavily => json_apis::tavily(query, &secret(env, "TAVILY_API_KEY")?),
        Provider::Exa => json_apis::exa(query, &secret(env, "EXA_API_KEY")?),
        Provider::Serper => json_apis::serper(query, &secret(env, "SERPER_API_KEY")?),
        Provider::Kagi => json_apis::kagi(query, &secret(env, "KAGI_API_KEY")?),
        Provider::Jina => json_apis::jina(query, &secret(env, "JINA_API_KEY")?),
        Provider::Bing => {
            json_apis::bing(query, &secret(env, "BING_API_KEY")?, &bing_endpoint(env)?)
        }
        Provider::Google => json_apis::google(
            query,
            &secret(env, "GOOGLE_API_KEY")?,
            &secret(env, "GOOGLE_CSE_ID")?,
        ),
        Provider::Perplexity => perplexity::build(query, &secret(env, "PERPLEXITY_API_KEY")?),
        Provider::Searxng => searxng::build(query, &searxng_endpoint(env)?),
        Provider::DuckDuckGo => {
            let mut url =
                search::DEFAULT_SEARCH_URL.replace("{query}", &percent_encode(query.text));
            if let Some(recency) = query.recency {
                // The HTML endpoint's date filter (`df`), as set by its own time selector.
                url.push_str(match recency {
                    Recency::Day => "&df=d",
                    Recency::Week => "&df=w",
                    Recency::Month => "&df=m",
                    Recency::Year => "&df=y",
                });
            }
            HttpRequest::get(url, Vec::new())
        }
    })
}

/// Pure: interpret a reply. The keyless provider parses HTML or its JSON variant; the others are JSON.
fn parse(provider: Provider, response: &Response) -> Result<Parsed, String> {
    match provider {
        Provider::DuckDuckGo => parse_keyless(response),
        _ => {
            let value: Value = serde_json::from_slice(&response.body)
                .map_err(|error| format!("the response is not valid JSON: {error}"))?;
            match provider {
                Provider::Brave => json_apis::parse_brave(&value),
                Provider::Tavily => json_apis::parse_tavily(&value),
                Provider::Exa => json_apis::parse_exa(&value),
                Provider::Serper => json_apis::parse_serper(&value),
                Provider::Kagi => json_apis::parse_kagi(&value),
                Provider::Jina => json_apis::parse_jina(&value),
                Provider::Google => json_apis::parse_google(&value),
                Provider::Bing => json_apis::parse_bing(&value),
                Provider::Perplexity => perplexity::parse(&value),
                Provider::Searxng => searxng::parse(&value),
                Provider::DuckDuckGo => parse_keyless(response),
            }
        }
    }
}

fn parse_keyless(response: &Response) -> Result<Parsed, String> {
    let kind = content::classify(response.media_type.as_deref(), &response.body)?;
    let page = content::decode(&response.body, response.charset.as_deref(), kind);
    let results = match kind {
        content::Kind::Json => {
            let value = serde_json::from_str(&page)
                .map_err(|error| format!("the search response is not valid JSON: {error}"))?;
            search::parse_json(&value)?
        }
        content::Kind::Html => search::parse_html(&page)?,
        content::Kind::Text => {
            return Err(
                "the search backend returned plain text, which is not a supported format".into(),
            );
        }
    };
    Ok(Parsed {
        answer: None,
        results,
    })
}

fn configured(env: Env, name: &str) -> bool {
    env(name).is_some_and(|value| !value.trim().is_empty())
}

/// `YYYY-MM-DD` for a count of days since the Unix epoch (proleptic Gregorian calendar).
pub fn iso_date(days: i64) -> String {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// A credential from the environment. The value is never echoed; errors name only the variable.
fn secret(env: Env, name: &str) -> Result<String, String> {
    let value = env(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is not set"))?;
    if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "{name} contains characters not allowed in an HTTP header"
        ));
    }
    Ok(value)
}

/// SearXNG base URL from `SEARXNG_URL`. Its host goes through the same SSRF guard as every request, so
/// loopback and private addresses are refused.
fn searxng_endpoint(env: Env) -> Result<String, String> {
    let base = env("SEARXNG_URL")
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "SEARXNG_URL is not set".to_owned())?;
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err("SEARXNG_URL must be an absolute http or https URL".into());
    }
    Ok(base)
}

/// Bing endpoint: the public API by default; `BING_SEARCH_ENDPOINT` selects an Azure resource. Keys
/// only travel over https, so an http endpoint is refused.
fn bing_endpoint(env: Env) -> Result<String, String> {
    match env("BING_SEARCH_ENDPOINT")
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
    {
        None => Ok(json_apis::BING_URL.to_owned()),
        Some(endpoint) if endpoint.starts_with("https://") => Ok(endpoint),
        Some(_) => Err("BING_SEARCH_ENDPOINT must be an https URL".into()),
    }
}

#[cfg(test)]
mod tests {
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
}

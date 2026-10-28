//! Search backends for `web_search`: keyed REST APIs, a self-hosted SearXNG, the keyless DuckDuckGo
//! HTML fallback and the keyless HTML scrapers (Startpage, Mojeek, Ecosia).
//!
//! Every provider is a pure request builder plus a pure response parser. [`search`] performs the
//! single HTTP exchange per attempt through the guarded [`Client`]. API keys are read from
//! environment variables, travel only in headers (Google CSE: its query string) to the provider's
//! own https origin, and never appear in results, errors or logs.

mod anthropic;
mod codex;
mod deepseek;
mod domains;
mod gemini;
mod grounded;
mod json_apis;
mod kimi;
mod ollama;
mod openai;
mod openrouter;
mod perplexity;
mod public;
mod scrapers;
mod searxng;
mod synthetic;
mod tinyfish;
mod xai;
pub use domains::DomainFilter;
pub use grounded::saved_key;

use super::content;
use super::fetch::{Client, Response, percent_encode};
use super::search::{self, SearchResult};
use serde_json::Value;
use std::path::Path;

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
    Firecrawl,
    Parallel,
    Zai,
    Xai,
    Startpage,
    Mojeek,
    Ecosia,
    Tinyfish,
    Deepseek,
    Kimi,
    Synthetic,
    Ollama,
    Public,
    OpenAi,
    Anthropic,
    Gemini,
    OpenRouter,
    Codex,
}

/// Every provider, in the order `names()` lists them. Providers added after the first release are
/// appended after the first [`ORIGINAL_COUNT`].
const ALL: [Provider; 29] = [
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
    Provider::Firecrawl,
    Provider::Parallel,
    Provider::Zai,
    Provider::Xai,
    Provider::Startpage,
    Provider::Mojeek,
    Provider::Ecosia,
    Provider::Tinyfish,
    Provider::Deepseek,
    Provider::Kimi,
    Provider::Synthetic,
    Provider::Ollama,
    Provider::Public,
    Provider::OpenAi,
    Provider::Anthropic,
    Provider::Gemini,
    Provider::OpenRouter,
    Provider::Codex,
];

/// Order used by `auto`: configured keyed providers first, then SearXNG, then the keyless DuckDuckGo
/// HTML endpoint (always available) and the keyless scrapers as further fallbacks. A scraper that hits a
/// bot wall fails with an error, so the next entry is tried.
const AUTO_ORDER: [Provider; 23] = [
    Provider::Exa,
    Provider::Brave,
    Provider::Tavily,
    Provider::Perplexity,
    Provider::Kagi,
    Provider::Jina,
    Provider::Serper,
    Provider::Firecrawl,
    Provider::Parallel,
    Provider::Zai,
    Provider::Xai,
    Provider::Tinyfish,
    Provider::Deepseek,
    Provider::Kimi,
    Provider::Synthetic,
    Provider::Ollama,
    Provider::Bing,
    Provider::Google,
    Provider::Searxng,
    Provider::DuckDuckGo,
    Provider::Startpage,
    Provider::Mojeek,
    Provider::Ecosia,
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
            Provider::Firecrawl => "firecrawl",
            Provider::Parallel => "parallel",
            Provider::Zai => "zai",
            Provider::Xai => "xai",
            Provider::Startpage => "startpage",
            Provider::Mojeek => "mojeek",
            Provider::Ecosia => "ecosia",
            Provider::Tinyfish => "tinyfish",
            Provider::Deepseek => "deepseek",
            Provider::Kimi => "kimi",
            Provider::Synthetic => "synthetic",
            Provider::Ollama => "ollama",
            Provider::Public => "public",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Gemini => "gemini",
            Provider::OpenRouter => "openrouter",
            Provider::Codex => "codex",
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
            Provider::Firecrawl => &["FIRECRAWL_API_KEY"],
            Provider::Parallel => &["PARALLEL_API_KEY"],
            Provider::Zai => &["ZAI_API_KEY"],
            Provider::Xai => &["XAI_API_KEY"],
            Provider::Tinyfish => &["TINYFISH_API_KEY"],
            Provider::Deepseek => &["DEEPSEEK_API_KEY"],
            Provider::Kimi => &["KIMI_SEARCH_API_KEY"],
            Provider::Synthetic => &["SYNTHETIC_API_KEY"],
            Provider::Ollama => &["OLLAMA_API_KEY"],
            Provider::OpenAi => &["OPENAI_API_KEY"],
            Provider::Anthropic => &["ANTHROPIC_API_KEY"],
            Provider::Gemini => &["GEMINI_API_KEY"],
            Provider::OpenRouter => &["OPENROUTER_API_KEY"],
            Provider::Codex => &[],
            Provider::DuckDuckGo
            | Provider::Startpage
            | Provider::Mojeek
            | Provider::Ecosia
            | Provider::Public => &[],
        }
    }

    fn from_name(name: &str) -> Option<Provider> {
        ALL.into_iter().find(|provider| provider.name() == name)
    }

    /// How this provider deviates from the requested `recency` window, if it does, so the caller can
    /// say so instead of presenting unfiltered results as recent ones.
    pub fn recency_note(self, recency: Recency) -> Option<String> {
        match (self, recency) {
            (
                Provider::Jina
                | Provider::Mojeek
                | Provider::Ecosia
                | Provider::Kimi
                | Provider::Synthetic
                | Provider::Ollama,
                _,
            ) => Some(format!(
                "{} does not support the recency filter, so results are not limited to the past {}",
                self.name(),
                recency.name()
            )),
            (Provider::Xai, _) => Some(format!(
                "xai applies the recency window as an instruction to the model, not a hard filter, so results may fall outside the past {}",
                recency.name()
            )),
            (
                Provider::OpenAi
                | Provider::Anthropic
                | Provider::Gemini
                | Provider::OpenRouter
                | Provider::Codex,
                _,
            ) => Some(format!(
                "{} applies the recency window as an instruction to the model, not a hard filter, so results may fall outside the past {}",
                self.name(),
                recency.name()
            )),
            (Provider::Public, _) => Some(format!(
                "public merges four engines, and mojeek and ecosia do not support the recency filter, so results may fall outside the past {}",
                recency.name()
            )),
            (Provider::Searxng, Recency::Week) => {
                Some("searxng has no week window, so results cover the past month".into())
            }
            _ => None,
        }
    }
}

/// Providers that existed when the `web_search` `provider` enum was first released; later ones are
/// appended to [`ALL`] after them.
const ORIGINAL_COUNT: usize = 11;

/// `auto` followed by the first `count` providers in [`ALL`] order.
fn names_prefix(count: usize) -> Vec<&'static str> {
    std::iter::once("auto")
        .chain(ALL.into_iter().take(count).map(Provider::name))
        .collect()
}

/// Names accepted by `web.search_provider` and the `provider` tool argument, `auto` first.
pub fn names() -> Vec<&'static str> {
    names_prefix(ALL.len())
}

/// Every `provider` enum a recorded `web_search` contract can carry: the first release, then each
/// later one, since each release only appended providers.
pub fn historical_names() -> Vec<Vec<&'static str>> {
    (ORIGINAL_COUNT..=ALL.len()).map(names_prefix).collect()
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
    pub filter: &'a domains::DomainFilter,
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
    pub domain_note: Option<String>,
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
    user_dir: &Path,
) -> Result<Found, String> {
    let mut failures = Vec::new();
    for &provider in chain {
        match attempt(client, provider, query, env, user_dir) {
            Ok(parsed) => {
                let present: Vec<SearchResult> = parsed
                    .results
                    .into_iter()
                    .filter(|result| !result.url.is_empty())
                    .collect();
                let (mut results, domain_note) = query.filter.apply(present);
                results.truncate(query.limit);
                return Ok(Found {
                    provider,
                    answer: parsed.answer,
                    results,
                    domain_note,
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

fn attempt(
    client: &Client,
    provider: Provider,
    query: &Query,
    env: Env,
    user_dir: &Path,
) -> Result<Parsed, String> {
    if provider == Provider::Public {
        return public::search(client, query, env, user_dir);
    }
    if provider == Provider::Codex {
        return codex::search(user_dir, query);
    }
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
    let mut parsed = parse(provider, &response)?;
    if provider == Provider::Gemini {
        gemini::resolve_redirects(client, &mut parsed.results);
    }
    Ok(parsed)
}

/// Pure: the exchange this provider needs for `query`, with keys from `env`.
pub fn build(provider: Provider, query: &Query, env: Env) -> Result<HttpRequest, String> {
    Ok(match provider {
        Provider::Tinyfish => tinyfish::build(query, &secret(env, "TINYFISH_API_KEY")?)?,
        Provider::Deepseek => deepseek::build(query, &secret(env, "DEEPSEEK_API_KEY")?)?,
        Provider::Kimi => kimi::build(query, &secret(env, "KIMI_SEARCH_API_KEY")?),
        Provider::Synthetic => synthetic::build(query, &secret(env, "SYNTHETIC_API_KEY")?),
        Provider::Ollama => ollama::build(query, &secret(env, "OLLAMA_API_KEY")?),
        Provider::OpenAi => openai::build(query, &secret(env, "OPENAI_API_KEY")?),
        Provider::Anthropic => anthropic::build(query, &secret(env, "ANTHROPIC_API_KEY")?),
        Provider::Gemini => gemini::build(query, &secret(env, "GEMINI_API_KEY")?),
        Provider::OpenRouter => openrouter::build(query, &secret(env, "OPENROUTER_API_KEY")?),
        Provider::Codex => {
            return Err("codex answers through the ChatGPT session, not an HTTP exchange".into());
        }
        Provider::Public => {
            return Err(
                "public fans out over the keyless engines and has no single request".into(),
            );
        }
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
        Provider::Firecrawl => json_apis::firecrawl(query, &secret(env, "FIRECRAWL_API_KEY")?),
        Provider::Parallel => json_apis::parallel(query, &secret(env, "PARALLEL_API_KEY")?),
        Provider::Zai => json_apis::zai(query, &secret(env, "ZAI_API_KEY")?),
        Provider::Xai => xai::build(query, &secret(env, "XAI_API_KEY")?),
        Provider::Startpage => scrapers::startpage(query),
        Provider::Mojeek => scrapers::mojeek(query),
        Provider::Ecosia => scrapers::ecosia(query),
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

/// Pure: interpret a reply. DuckDuckGo parses HTML or its JSON variant, the scrapers parse HTML only,
/// and the keyed providers are JSON.
fn parse(provider: Provider, response: &Response) -> Result<Parsed, String> {
    let json = || -> Result<Value, String> {
        serde_json::from_slice(&response.body)
            .map_err(|error| format!("the response is not valid JSON: {error}"))
    };
    match provider {
        Provider::DuckDuckGo => parse_keyless(response),
        Provider::Startpage | Provider::Mojeek | Provider::Ecosia => Ok(Parsed {
            answer: None,
            results: scrapers::parse(provider, &html_page(response, provider)?)?,
        }),
        Provider::Brave => json_apis::parse_brave(&json()?),
        Provider::Tavily => json_apis::parse_tavily(&json()?),
        Provider::Exa => json_apis::parse_exa(&json()?),
        Provider::Serper => json_apis::parse_serper(&json()?),
        Provider::Kagi => json_apis::parse_kagi(&json()?),
        Provider::Jina => json_apis::parse_jina(&json()?),
        Provider::Google => json_apis::parse_google(&json()?),
        Provider::Bing => json_apis::parse_bing(&json()?),
        Provider::Firecrawl => json_apis::parse_firecrawl(&json()?),
        Provider::Parallel => json_apis::parse_parallel(&json()?),
        Provider::Zai => json_apis::parse_zai(&json()?),
        Provider::Perplexity => perplexity::parse(&json()?),
        Provider::Xai => xai::parse(&json()?),
        Provider::Searxng => searxng::parse(&json()?),
        Provider::Tinyfish => tinyfish::parse(&json()?),
        Provider::Deepseek => deepseek::parse(&json()?),
        Provider::Kimi => kimi::parse(&json()?),
        Provider::Synthetic => synthetic::parse(&json()?),
        Provider::Ollama => ollama::parse(&json()?),
        Provider::OpenAi => openai::parse(&json()?),
        Provider::Anthropic => anthropic::parse(&json()?),
        Provider::Gemini => gemini::parse(&json()?),
        Provider::OpenRouter => openrouter::parse(&json()?),
        Provider::Codex => Err("codex parses its own stream, not an HTTP reply".into()),
        Provider::Public => Err("public parses each engine's reply separately".into()),
    }
}

/// The decoded HTML body of a scraped search page; any other media type is an error.
fn html_page(response: &Response, provider: Provider) -> Result<String, String> {
    let kind = content::classify(response.media_type.as_deref(), &response.body)?;
    if kind != content::Kind::Html {
        return Err(format!(
            "{} returned a non-HTML response, so it cannot be parsed",
            provider.name()
        ));
    }
    Ok(content::decode(
        &response.body,
        response.charset.as_deref(),
        kind,
    ))
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
mod tests;

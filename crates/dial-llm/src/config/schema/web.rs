//! Web tool setting declarations (`web_fetch`, `web_search`). The `web` table keeps its structured
//! editor under Tools → Configuration for the reader chain and search endpoint.
use super::*;

pub(super) static DECLARATIONS: &[SettingDef] = &[
    field!(
        "web.timeout_secs",
        web.timeout_secs,
        TO,
        "Web",
        "Request timeout",
        "Seconds one web_fetch or web_search request may take in total, redirects included. Raise it for slow documentation hosts; reading a URL through `read` keeps the 30-second default.",
        Kind::Int {
            min: 1,
            max: 600,
            step: 5
        },
    ),
    field!(
        "web.search_provider",
        web.search_provider,
        TO,
        "Web",
        "Search provider",
        "Backend for web_search: `auto` (or empty) tries every provider whose API key is in the environment, then keyless engines. Name one (brave, tavily, exa, searxng, duckduckgo, ...) to pin it. Ignored when a search URL is set.",
        Kind::Text,
    ),
    field!(
        "web.jina_reader",
        web.jina_reader,
        TO,
        "Web",
        "Allow Jina reader",
        "Allow the `jina` page reader, which sends each fetched URL to r.jina.ai. Off keeps page URLs on this machine and the target host.",
        Kind::Bool,
    ),
];

use super::*;
use clap::Parser;

#[derive(Parser)]
struct Harness {
    #[command(flatten)]
    search: SearchArgs,
}

fn parse(argv: &[&str]) -> Result<SearchArgs, clap::Error> {
    let mut full = vec!["search"];
    full.extend_from_slice(argv);
    Harness::try_parse_from(full).map(|harness| harness.search)
}

fn known() -> Vec<String> {
    ["auto", "duckduckgo", "brave", "searxng"]
        .iter()
        .map(|name| name.to_string())
        .collect()
}

#[test]
fn parses_flags_and_joins_positional_query() {
    let args = parse(&[
        "--model=web/duckduckgo",
        "--recency",
        "week",
        "-l",
        "5",
        "--compact",
        "rust",
        "async",
    ])
    .unwrap();
    assert_eq!(args.model.as_deref(), Some("web/duckduckgo"));
    assert_eq!(args.recency.as_deref(), Some("week"));
    assert_eq!(args.limit, Some(5));
    assert!(args.compact);
    assert_eq!(args.query.join(" "), "rust async");
}

#[test]
fn rejects_missing_query_and_bad_values() {
    assert!(parse(&[]).is_err());
    assert!(parse(&["--recency", "decade", "x"]).is_err());
    assert!(parse(&["--limit", "many", "x"]).is_err());
}

#[test]
fn query_directives_pass_through_untouched() {
    let args = parse(&["transformer site:arxiv.org -site:reddit.com"]).unwrap();
    let arguments = tool_arguments(&args, &known()).unwrap();
    assert_eq!(
        arguments["query"],
        "transformer site:arxiv.org -site:reddit.com"
    );
}

#[test]
fn maps_flags_onto_tool_arguments() {
    let args = parse(&[
        "--model=web/Brave",
        "--recency=day",
        "--limit=3",
        "latest",
        "news",
    ])
    .unwrap();
    let arguments = tool_arguments(&args, &known()).unwrap();
    assert_eq!(
        arguments,
        json!({"query": "latest news", "limit": 3, "recency": "day", "provider": "brave"})
    );
}

#[test]
fn omits_unset_options() {
    let args = parse(&["solo"]).unwrap();
    assert_eq!(
        tool_arguments(&args, &known()).unwrap(),
        json!({"query": "solo"})
    );
}

#[test]
fn resolves_provider_selectors() {
    assert_eq!(
        resolve_provider("web/searxng", &known()).unwrap(),
        "searxng"
    );
    assert_eq!(resolve_provider("auto", &known()).unwrap(), "auto");
    let error = resolve_provider("openrouter/google/gemini", &known()).unwrap_err();
    assert!(error.contains("unknown search model"), "{error}");
    assert!(error.contains("duckduckgo, brave"), "{error}");
}

#[test]
fn reads_provider_enum_from_schema() {
    let schema = json!({"function": {"parameters": {"properties": {
        "provider": {"enum": ["auto", "duckduckgo"]}
    }}}});
    assert_eq!(known_providers(&schema), vec!["auto", "duckduckgo"]);
    assert!(known_providers(&json!({})).is_empty());
}

#[test]
fn compact_caps_answer_block_only() {
    let answer: String = (1..=8).map(|n| format!("line {n}\n")).collect();
    let text = format!(
        "[Untrusted]\n\nAnswer: {}\n\n2 results for \"x\":\n\n1. A\n   u",
        answer.trim_end()
    );
    let compacted = compact(&text);
    assert!(compacted.contains("Answer: line 1"));
    assert!(compacted.contains("line 6\n…\n\n2 results for"));
    assert!(!compacted.contains("line 7"));
    assert!(compacted.ends_with("1. A\n   u"));
}

#[test]
fn compact_leaves_short_answers_and_plain_results_alone() {
    let short = "Answer: one\ntwo\n\n1 results for \"x\":\n\n1. A";
    assert_eq!(compact(short), short);
    let plain = "1 results for \"x\":\n\n1. A";
    assert_eq!(compact(plain), plain);
}

//! `dial search` (aliases `q`, `web-search`): run one web search through Dial's `web_search` tool
//! and print the results.
//!
//! Wiring (main.rs): add a subcommand variant `#[command(alias = "q", alias = "web-search")] Search(SearchArgs)`
//! and call [`run`]. The exit code is the return value.
//!
//! `--model <selector>` names a search provider. Dial has no LLM-backed catalog
//! for search; it has the provider chain in `web.search_provider`. `web/<name>` and `<name>` both select
//! the provider `<name>`, validated against the `provider` enum of the `web_search` schema. `--recency`,
//! `--limit` caps the result count and `--compact` condenses the output. Directives in the query (`site:`, `after:`, quotes,
//! `-term`, `OR`) are interpreted by the provider layer, not by this command.
//!
//! Approval: the `web_search` tool asks for approval inside a session. This command is an explicit
//! user invocation, so it does not prompt; the sandbox network policy is still enforced by the tool.

use crate::config::types::Config;
use crate::storage::paths::Layout;
use crate::tools::ext::ToolContext;
use crate::tools::{Workspace, web};
use anyhow::Result;
use serde_json::{Value, json};

const TOOL_NAME: &str = "web_search";
const RECENCY_VALUES: [&str; 4] = ["day", "week", "month", "year"];
/// The compact rendering caps the synthesized answer at this many lines.
const COMPACT_ANSWER_LINES: usize = 6;

#[derive(Debug, Clone, clap::Args)]
#[command(
    after_help = "Query directives:\n  site:/-site:  after:/before: (YYYY-MM-DD)  inurl:  intitle:  filetype:\n  \"exact phrase\"  -term  OR\n  Mapped to native provider filters where available, otherwise applied as a\n  lenient post-filter (a constraint matching nothing is relaxed, not fatal).\n\nExamples:\n  dial q --model=web/duckduckgo \"what's the color of the sky\"\n  dial q --model=brave --recency=week \"latest TypeScript changes\"\n  dial q 'transformer scaling site:arxiv.org after:2024 -site:reddit.com'"
)]
pub struct SearchArgs {
    /// Search provider: a provider name such as duckduckgo or brave, or web/<name>.
    #[arg(long, value_name = "SELECTOR")]
    pub model: Option<String>,
    /// Restrict results to a recent window (when the provider supports it).
    #[arg(long, value_parser = ["day", "week", "month", "year"])]
    pub recency: Option<String>,
    /// Maximum results to return (default 8, max 20).
    #[arg(short = 'l', long, value_name = "N")]
    pub limit: Option<u64>,
    /// Condensed output: cap the synthesized answer at six lines.
    #[arg(long)]
    pub compact: bool,
    /// Search query text; separate words are joined with spaces.
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,
}

/// Provider names the `web_search` schema accepts (`auto` first). Empty when the tool is missing.
pub fn known_providers(schema: &Value) -> Vec<String> {
    schema
        .pointer("/function/parameters/properties/provider/enum")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Map a `--model` selector onto a Dial provider name. `web/` is optional.
pub fn resolve_provider(selector: &str, known: &[String]) -> Result<String, String> {
    let name = selector.trim();
    let name = name.strip_prefix("web/").unwrap_or(name);
    known
        .iter()
        .find(|provider| provider.eq_ignore_ascii_case(name))
        .cloned()
        .ok_or_else(|| {
            format!(
                "unknown search model {selector:?}; use one of: {}",
                known.join(", ")
            )
        })
}

/// The `web_search` tool arguments for these flags. Errors name the offending input.
pub fn tool_arguments(args: &SearchArgs, known: &[String]) -> Result<Value, String> {
    let query = args.query.join(" ");
    let query = query.trim();
    if query.is_empty() {
        return Err("Query is required".into());
    }
    let mut arguments = json!({ "query": query });
    if let Some(limit) = args.limit {
        arguments["limit"] = json!(limit);
    }
    if let Some(recency) = &args.recency {
        if !RECENCY_VALUES.contains(&recency.as_str()) {
            return Err(format!(
                "Invalid recency {recency:?}; valid values: {}",
                RECENCY_VALUES.join(", ")
            ));
        }
        arguments["recency"] = json!(recency);
    }
    if let Some(model) = &args.model {
        arguments["provider"] = json!(resolve_provider(model, known)?);
    }
    Ok(arguments)
}

fn is_results_header(line: &str) -> bool {
    line.starts_with("No results for ")
        || line
            .split_once(" results for ")
            .is_some_and(|(count, _)| count.parse::<usize>().is_ok())
}

/// Cap the `Answer:` block at [`COMPACT_ANSWER_LINES`] lines, marking the cut with an ellipsis.
/// Text without an answer block is returned unchanged.
pub fn compact(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|line| line.starts_with("Answer: ")) else {
        return text.to_owned();
    };
    let header = lines[start..]
        .iter()
        .position(|line| is_results_header(line))
        .map_or(lines.len(), |offset| start + offset);
    let mut end = header;
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let answer = &lines[start..end];
    if answer.len() <= COMPACT_ANSWER_LINES {
        return text.to_owned();
    }
    let mut out: Vec<&str> = lines[..start].to_vec();
    out.extend_from_slice(&answer[..COMPACT_ANSWER_LINES]);
    out.push("…");
    out.extend_from_slice(&lines[end..]);
    out.join("\n")
}

fn fail(message: &str) -> i32 {
    eprintln!("Error: {message}");
    1
}

/// Run one search and print it. Returns the process exit code: 0 on results (including "no
/// results"), 1 on a usage, configuration or search error.
pub fn run(layout: &Layout, config: &Config, args: &SearchArgs) -> Result<i32> {
    let tools = web::tools(config);
    let Some(tool) = tools.iter().find(|tool| tool.name() == TOOL_NAME) else {
        return Ok(fail("the web_search tool is not available"));
    };
    let known = known_providers(&tool.schema());
    let arguments = match tool_arguments(args, &known) {
        Ok(arguments) => arguments,
        Err(message) => return Ok(fail(&message)),
    };
    let workspace = Workspace::new(&layout.project_root)?;
    let output = tool.call(
        &ToolContext {
            workspace: &workspace,
            config,
            mutation_authorizer: None,
            confirm: None,
        },
        &arguments,
    );
    if output.is_error {
        eprintln!("{}", output.text);
        return Ok(1);
    }
    let text = if args.compact {
        compact(&output.text)
    } else {
        output.text
    };
    println!("{text}");
    Ok(0)
}

#[cfg(test)]
mod tests {
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
}

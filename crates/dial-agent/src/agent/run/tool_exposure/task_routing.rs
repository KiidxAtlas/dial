//! Task-scoped schema selection. Routing is local and deterministic; it never executes a tool,
//! reads project files, calls a model, or widens an authorization boundary.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

const TOOL_LIMIT: usize = 6;
const SCHEMA_BYTES: usize = 24 * 1024;
const LEAN_TOOL_LIMIT: usize = 2;
const LEAN_SCHEMA_BYTES: usize = 8 * 1024;
const CONTEXT_LIMIT: usize = 24;

#[derive(Default)]
pub(in crate::agent) struct ToolRouting {
    pub(in crate::agent) selected: Vec<String>,
    query: String,
    context: BTreeSet<String>,
    suppressed: BTreeSet<String>,
    followed: BTreeSet<String>,
}
impl ToolRouting {
    pub(in crate::agent) fn unload(&mut self, names: &[String]) {
        self.selected.retain(|name| !names.contains(name));
        self.suppressed.extend(names.iter().cloned());
    }
}

fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| match word.to_lowercase().as_str() {
            "docs" => "documentation".into(),
            "libraries" => "library".into(),
            "dependencies" => "dependency".into(),
            "apis" => "api".into(),
            "pages" => "page".into(),
            "modules" => "module".into(),
            "logs" => "log".into(),
            "migrations" => "migration".into(),
            "projects" => "project".into(),
            "tables" => "table".into(),
            other => other.to_owned(),
        })
        .collect()
}
fn any(terms: &BTreeSet<String>, choices: &[&str]) -> bool {
    choices.iter().any(|term| terms.contains(*term))
}
fn parts(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("mcp__")?.split_once("__")
}
fn entrypoint(name: &str) -> Option<(bool, &str)> {
    let (_, tool) = parts(name)?;
    let terms = words(tool);
    any(&terms, &["list", "search", "resolve", "query"])
        .then(|| (!any(&terms, &["documentation", "library"]), name))
}
fn coding(terms: &BTreeSet<String>) -> bool {
    any(
        terms,
        &[
            "fix",
            "implement",
            "integrate",
            "configure",
            "debug",
            "migrate",
            "upgrade",
            "error",
            "fails",
            "broken",
            "build",
            "refactor",
            "add",
            "develop",
        ],
    )
}

/// Workflow-specific native tools. A task needs an actual intent, not just an incidental word in
/// a filename. The eligible catalog still decides which of these capabilities exists.
fn native(terms: &BTreeSet<String>) -> Vec<&'static str> {
    let mut names = Vec::new();
    // Web schemas are not a coding core. Offer them directly for web intent rather than
    // making research pay for a discovery round, or every unrelated coding step pay for them.
    if any(terms, &["web", "online", "internet", "research", "browse"]) {
        names.extend(["web_search", "web_fetch"]);
    } else if any(terms, &["http", "https", "url", "website"]) {
        names.push("web_fetch");
    }
    if any(terms, &["screenshot", "playwright", "browser"])
        || (any(terms, &["ui", "frontend", "layout", "css", "page"])
            && any(
                terms,
                &[
                    "visual", "render", "rendered", "design", "redesign", "verify", "inspect",
                ],
            ))
    {
        names.push("browser");
    }
    if any(terms, &["dependency", "crate", "package", "library", "sdk"])
        && any(terms, &["source", "implementation", "internals"])
    {
        names.push("dep_source");
    }
    if (any(terms, &["structural", "ast", "syntax", "pattern"])
        && any(
            terms,
            &[
                "search", "find", "match", "replace", "rewrite", "refactor", "tree", "node",
            ],
        ))
        || (any(terms, &["callers", "usages", "occurrences"])
            && any(terms, &["replace", "rewrite", "refactor"]))
    {
        names.push("ast_search");
        if any(terms, &["replace", "rewrite", "refactor"]) {
            names.push("ast_edit");
        }
    }
    if any(
        terms,
        &[
            "rename",
            "references",
            "definition",
            "definitions",
            "implementations",
            "implementation",
            "callers",
            "usages",
            "hover",
            "signature",
            "diagnostics",
        ],
    ) && any(
        terms,
        &[
            "symbol",
            "symbols",
            "function",
            "method",
            "class",
            "type",
            "struct",
            "trait",
            "interface",
            "enum",
            "module",
        ],
    ) {
        names.push("lsp");
    }
    if any(terms, &["previous", "earlier", "past", "prior", "archived"])
        && any(
            terms,
            &["conversation", "session", "discussed", "turn", "transcript"],
        )
    {
        names.push("session_search");
    }
    if any(
        terms,
        &["history", "regression", "yesterday", "blame", "recent"],
    ) && any(
        terms,
        &[
            "changed",
            "changes",
            "change",
            "introduced",
            "diff",
            "broke",
            "modified",
        ],
    ) {
        names.push("code_history");
    }
    if any(terms, &["breakpoint", "breakpoints"])
        || (terms.contains("debug") && any(terms, &["crash", "stack", "runtime"]))
    {
        names.push("dap");
    }
    // Creating pictures, not containers or image-processing code.
    if any(
        terms,
        &["image", "picture", "illustration", "logo", "artwork"],
    ) && any(terms, &["generate", "create", "draw", "make", "design"])
        && !any(terms, &["docker", "container", "dockerfile", "oci"])
    {
        names.push("image_gen");
    }
    names
}

fn ranked(query: &str, candidates: Vec<(String, String)>) -> Vec<String> {
    crate::agent::model::assistance::discover(query, candidates)
        .into_iter()
        .filter(|entry| entry.2)
        .map(|entry| entry.0)
        .collect()
}

fn select(query: &str, context: &BTreeSet<String>, candidates: &[(String, String)]) -> Vec<String> {
    let terms = words(query);
    let mut selected = Vec::new();
    // Explicit tool names are strong evidence even if no workflow rule covers that specialist.
    selected.extend(
        candidates
            .iter()
            .filter(|(name, _)| {
                query
                    .split_whitespace()
                    .any(|word| word.trim_matches(['`', ',', '.', '"']) == name)
            })
            .map(|entry| entry.0.clone()),
    );
    selected.extend(
        native(&terms)
            .into_iter()
            .filter(|name| candidates.iter().any(|entry| entry.0 == *name))
            .map(str::to_owned),
    );
    let mut domain_terms = terms.clone();
    if coding(&terms) {
        for package in context {
            domain_terms.extend(words(package));
        }
    }
    let mut servers: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (name, text) in candidates {
        let Some((server, _)) = parts(name) else {
            continue;
        };
        let server_words = words(server);
        if server.len() >= 3 && !server_words.is_empty() && server_words.is_subset(&domain_terms) {
            servers
                .entry(server.into())
                .or_default()
                .push((name.clone(), text.clone()));
        }
    }
    for (server, tools) in servers {
        let server_words = words(&server);
        let capability = terms
            .difference(&server_words)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let matches = ranked(&capability, tools.clone());
        if matches.is_empty() {
            if let Some((_, name)) = tools.iter().filter_map(|(name, _)| entrypoint(name)).min() {
                selected.push(name.to_owned());
            }
        } else {
            selected.extend(matches);
        }
    }
    // General library-documentation services are useful without the user knowing the server's
    // name. Require a library lookup capability; a vendor-specific docs server is not a substitute.
    let library_task = any(
        &terms,
        &["library", "framework", "sdk", "api", "crate", "package"],
    ) || (coding(&terms) && !context.is_empty());
    if library_task {
        let docs = candidates
            .iter()
            .filter(|(name, text)| {
                let indexed = words(text);
                parts(name).is_some()
                    && indexed.contains("documentation")
                    && any(&indexed, &["library", "libraryid", "libraryname"])
            })
            .cloned()
            .collect();
        // Offer both halves of lookup -> query, rather than requiring an extra discovery round
        // solely because the lookup tool's name scores higher than its consumer.
        selected.extend(
            crate::agent::model::assistance::discover("library documentation", docs)
                .into_iter()
                .take(2)
                .map(|entry| entry.0),
        );
    }
    // Domain-neutral MCP routing requires two non-boilerplate task terms. A single shared word
    // such as 'search', 'file' or 'fix' must not expose an unrelated remote service.
    let mut specific = terms;
    specific.retain(|term| {
        ![
            "a",
            "an",
            "the",
            "and",
            "or",
            "to",
            "for",
            "of",
            "in",
            "on",
            "with",
            "this",
            "that",
            "my",
            "i",
            "it",
            "is",
            "please",
            "need",
            "want",
            "help",
            "use",
            "using",
            "tool",
            "tools",
            "read",
            "get",
            "list",
            "search",
            "find",
            "file",
            "files",
            "fix",
            "code",
            "project",
            "task",
            "documentation",
        ]
        .contains(&term.as_str())
    });
    let relevant = candidates
        .iter()
        .filter(|(name, text)| {
            parts(name).is_some() && specific.intersection(&words(text)).count() >= 2
        })
        .cloned()
        .collect();
    selected.extend(ranked(
        &specific.iter().cloned().collect::<Vec<_>>().join(" "),
        relevant,
    ));
    // An explicit inspection request with a distinctive capability in exactly one server's tool
    // names is useful even when the user does not know that server's name. Generic verbs and
    // ambiguous capabilities still require stronger evidence.
    if any(
        &words(query),
        &["inspect", "show", "check", "list", "audit", "view"],
    ) {
        let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, _) in candidates {
            if let Some((server, action)) = parts(name) {
                for term in words(action).intersection(&specific) {
                    if term.len() >= 4 || term == "log" {
                        owners
                            .entry(term.clone())
                            .or_default()
                            .insert(server.to_owned());
                    }
                }
            }
        }
        let relevant = candidates
            .iter()
            .filter(|(name, _)| {
                let Some((server, action)) = parts(name) else {
                    return false;
                };
                words(action).iter().any(|term| {
                    owners
                        .get(term)
                        .is_some_and(|servers| servers.len() == 1 && servers.contains(server))
                })
            })
            .cloned()
            .collect();
        selected.extend(ranked(query, relevant));
    }
    let mut seen = BTreeSet::new();
    selected.retain(|name| seen.insert(name.clone()));
    selected
}

/// A successful discovery makes same-server consumers of the discovered resource relevant.
/// Match schemas, never instructions or tool names supplied in untrusted result prose.
fn followups(source: &str, candidates: &[(String, String)]) -> Vec<String> {
    let Some((server, action)) = parts(source) else {
        return Vec::new();
    };
    if entrypoint(source).is_none() {
        return Vec::new();
    }
    let mut resources = words(action);
    resources.retain(|word| {
        ![
            "list", "search", "resolve", "query", "get", "read", "id", "by",
        ]
        .contains(&word.as_str())
    });
    if resources.is_empty() {
        return Vec::new();
    }
    let candidates = candidates
        .iter()
        .filter(|(name, text)| {
            let Some((other_server, tool)) = parts(name) else {
                return false;
            };
            server == other_server
                && any(&words(tool), &["get", "read", "query"])
                && !resources.is_disjoint(&words(text))
        })
        .cloned()
        .collect();
    ranked(
        &resources.into_iter().collect::<Vec<_>>().join(" "),
        candidates,
    )
}

impl Agent<'_> {
    pub(in crate::agent) fn begin_tool_routing(&mut self, query: &str) {
        self.tool_routing = ToolRouting {
            query: crate::agent::window::prompt::truncate_chars(query, 4096),
            ..Default::default()
        };
        self.refresh_task_tools();
    }

    pub(in crate::agent) fn observe_tool_routing(&mut self, call: &ToolCall, output: &ToolOutput) {
        if self.tool_routing.query.is_empty()
            || !self.automatic_tool_routing()
            || output.is_error
            || crate::runtime::execution::ToolStatus::from_output(output)
                != crate::runtime::execution::ToolStatus::Ok
        {
            return;
        }
        let signals = super::task_context::signals(call, output);
        let before = self.tool_routing.context.len();
        for signal in signals {
            if self.tool_routing.context.len() >= CONTEXT_LIMIT {
                break;
            }
            self.tool_routing.context.insert(signal);
        }
        let mut changed = self.tool_routing.context.len() != before && self.refresh_task_tools();
        if parts(&call.name).is_some()
            && entrypoint(&call.name).is_some()
            && self.tool_routing.followed.len() < CONTEXT_LIMIT
            && self.tool_routing.followed.insert(call.name.clone())
        {
            let matches = followups(&call.name, &self.task_tool_candidates());
            changed |= self.add_task_tools(matches, true);
        }
        if changed {
            self.rebuild_system();
        }
    }

    fn automatic_tool_routing(&self) -> bool {
        !self.hermetic
            && !self.lean_tools
            && self.config.harness.tools == config::tools::default_tools()
    }

    fn task_tool_candidates(&self) -> Vec<(String, String)> {
        let offered = self.offered();
        self.catalog()
            .into_iter()
            .filter_map(|name| {
                if offered.contains(&name)
                    || self.tool_routing.suppressed.contains(&name)
                    || !self.tool_eligible(&name)
                {
                    return None;
                }
                let schema = self.schemas(std::slice::from_ref(&name)).ok()?.pop()?;
                Some((name, super::super::dispatch::search_index::text(&schema)))
            })
            .collect()
    }

    fn refresh_task_tools(&mut self) -> bool {
        if !self.automatic_tool_routing() {
            return false;
        }
        let candidates = self.task_tool_candidates();
        let mut matches = select(
            &self.tool_routing.query,
            &self.tool_routing.context,
            &candidates,
        );
        // Small contexts retain the core offer and route only the requested MCP capabilities.
        if self.lean_local() || self.minimal_tools() {
            matches.retain(|name| parts(name).is_some());
        }
        self.add_task_tools(matches, false)
    }

    fn add_task_tools(&mut self, matches: Vec<String>, followup: bool) -> bool {
        let before = self.tool_routing.selected.len();
        let (tool_limit, schema_limit) = if self.lean_local() || self.minimal_tools() {
            (LEAN_TOOL_LIMIT, LEAN_SCHEMA_BYTES)
        } else {
            (TOOL_LIMIT, SCHEMA_BYTES)
        };
        // One follow-up can use an additional slot, but not additional schema bytes.
        let tool_limit = tool_limit + usize::from(followup);
        let mut bytes = self
            .schemas(&self.tool_routing.selected)
            .map(|schemas| {
                schemas
                    .iter()
                    .map(|schema| schema.to_string().len())
                    .sum::<usize>()
            })
            .unwrap_or(schema_limit);
        // Monotonic within a turn: new evidence may add tools, but never churn existing schemas.
        // Explicit unloads are suppressed until the next external request.
        for name in matches {
            if self.tool_routing.selected.len() >= tool_limit {
                break;
            }
            let Some(schema) = self
                .schemas(std::slice::from_ref(&name))
                .ok()
                .and_then(|mut s| s.pop())
            else {
                continue;
            };
            let size = schema.to_string().len();
            if size <= schema_limit.saturating_sub(bytes) {
                bytes += size;
                self.tool_routing.selected.push(name);
            }
        }
        self.tool_routing.selected.len() != before
    }
}

#[cfg(test)]
mod tests;

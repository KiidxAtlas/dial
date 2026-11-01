//! Task-scoped schema selection. Routing is local and deterministic; it never executes a tool,
//! reads project files, calls a model, or widens an authorization boundary.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

const TOOL_LIMIT: usize = 6;
const SCHEMA_BYTES: usize = 24 * 1024;
const LEAN_TOOL_LIMIT: usize = 2;
const LEAN_SCHEMA_BYTES: usize = 8 * 1024;
const CONTEXT_LIMIT: usize = 24;
/// Native web pair offered together for research intent. Small profiles keep it ahead of MCP matches.
const WEB_TOOLS: [&str; 2] = ["web_search", "web_fetch"];

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
    /// Whether the user explicitly unloaded `name`; such a tool is not re-offered by gap routing.
    pub(in crate::agent) fn is_suppressed(&self, name: &str) -> bool {
        self.suppressed.contains(name)
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
/// Request prose for intent rules: code spans and identifier-like tokens (`$request_method`,
/// `server.py`, `/usr/bin`) name things the task touches, not capabilities it asks for. A URL
/// still signals URL intent.
fn prose_words(query: &str) -> BTreeSet<String> {
    let text: String = query
        .split('`')
        .step_by(2)
        .flat_map(str::split_whitespace)
        .map(|token| token.trim_end_matches(['.', ',', ';', ':', '?', '!', ')']))
        .filter_map(|token| {
            if token.contains("://") {
                Some("url")
            } else {
                (!token.contains(['_', '$', '/', '\\', '.', '='])).then_some(token)
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    words(&text)
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

/// Closing delimiter for a quote that opens a whitespace-separated name, e.g. `"logs, q1.csv"`.
fn closing_quote(open: char) -> Option<char> {
    match open {
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        '“' => Some('”'),
        '‘' => Some('’'),
        _ => None,
    }
}

fn ends_quote(chunk: &str, close: char) -> bool {
    chunk
        .trim_end_matches([',', ';', ':', '?', '!', '(', ')', '.'])
        .ends_with(close)
}

/// Source formats in filenames are useful signals; incidental module names (`audio.rs`) are not.
fn source_tools(query: &str) -> Vec<&'static str> {
    const INSPECTION_VERBS: &[&str] = &[
        "inspect",
        "read",
        "analyze",
        "analyse",
        "summarize",
        "summarise",
        "extract",
        "compare",
        "verify",
        "check",
        "audit",
        "investigate",
        "search",
        "find",
        "locate",
        "calculate",
        "compute",
        "count",
        "plot",
        "measure",
        "explain",
        "answer",
        "query",
    ];
    let mut source_file = false;
    let mut supplied_input = false;
    let mut text = Vec::new();
    // Inspection elsewhere in a request must not turn a deliverable's format into an input.
    // Keep this lexical and conservative: explicit tool names are handled separately by select.
    for clause in query.split(['\n', ';']).flat_map(|line| line.split(". ")) {
        let mut output_only = false;
        let mut created_output = false;
        let mut previous = "";
        let mut after_comma = false;
        let mut quote = None;
        let mut chunks = clause.split_whitespace();
        while let Some(chunk) = chunks.next() {
            // Only paired quote delimiters protect a literal name. Apostrophe-led prose such as
            // "'til noon" must not hide the next output clause, nor may "don't" close a filename.
            let mut body = chunk.trim_start_matches('(');
            let mut quoted = quote.is_some();
            let first = body.chars().next();
            if let (None, Some(open), Some(close)) = (quote, first, first.and_then(closing_quote)) {
                let tail = &body[open.len_utf8()..];
                if ends_quote(tail, close) || chunks.clone().any(|part| ends_quote(part, close)) {
                    quote = Some(close);
                    body = tail;
                    quoted = true;
                }
            }
            if quote.is_some_and(|close| ends_quote(body, close)) {
                quote = None;
            }
            let after_previous_comma =
                std::mem::replace(&mut after_comma, chunk.ends_with(',') && quote.is_none());
            let token = chunk.trim_matches([
                '`', '"', '\'', '“', '”', '‘', '’', ',', ';', ':', '?', '!', '(', ')', '.',
            ]);
            if token.is_empty() {
                continue;
            }
            let word = token.to_ascii_lowercase();
            // Creation words can also be nouns ("inspect the export of sales.csv"). Only
            // clause-leading, connector-led or comma-separated verbs introduce an output target,
            // and never a word inside a quoted name.
            let creation_verb = !quoted
                && [
                    "write", "export", "save", "create", "generate", "produce", "output",
                ]
                .contains(&word.as_str())
                && (previous.is_empty()
                    || after_previous_comma
                    || ["and", "then", "please", "to"]
                        .iter()
                        .any(|connector| previous.eq_ignore_ascii_case(connector)));
            previous = token;
            if creation_verb {
                output_only = true;
                created_output = true;
                continue;
            }
            if created_output && ["as", "in", "into", "to"].contains(&word.as_str()) {
                output_only = true;
                continue;
            }
            if ["from", "using", "given", "input", "of", "on", "about"].contains(&word.as_str()) {
                supplied_input |= created_output;
                output_only = false;
            }
            let (extension, filename) = token
                .rsplit_once('.')
                .map_or((token, false), |(_, extension)| (extension, true));
            let format = match extension.to_ascii_lowercase().as_str() {
                "pdf" => Some("pdf"),
                "docx" => Some("docx"),
                "pptx" => Some("pptx"),
                "xlsx" => Some("xlsx"),
                "csv" => Some("csv"),
                "json" | "jsonl" => Some("json"),
                "wav" | "mp3" | "m4a" | "flac" | "ogg" => Some("audio"),
                "png" | "jpg" | "jpeg" | "webp" | "tiff" => Some("image"),
                "har" => Some("har"),
                "sqlite" | "sqlite3" | "db" => Some("sqlite"),
                "folded" => Some("profile"),
                _ => None,
            };
            // Suppress only an output target phrase, not the rest of a creation clause:
            // "write a script to parse sales.csv" must retain its genuine input.
            if output_only
                && (format.is_some()
                    || [
                        "a",
                        "an",
                        "the",
                        "as",
                        "in",
                        "into",
                        "to",
                        "new",
                        "file",
                        "format",
                        "named",
                        "called",
                        "summary",
                        "report",
                        "document",
                        "spreadsheet",
                        "workbook",
                        "presentation",
                        "slides",
                        "table",
                        "chart",
                        "image",
                        "audio",
                        "data",
                        "dataset",
                    ]
                    .contains(&word.as_str()))
            {
                if filename {
                    output_only = false;
                }
                continue;
            }
            output_only = false;
            if filename {
                source_file |= format.is_some();
                if let Some(format) = format {
                    text.push(format);
                }
            } else if !token.contains(['/', '\\']) {
                text.push(token);
            }
        }
    }
    let terms = words(&text.join(" "));
    let inspect = any(&terms, INSPECTION_VERBS) || (supplied_input && source_file);
    // Questions about supplied captures should not require tool names or inspection jargon.
    // Keep conceptual questions and requests to implement a parser out of this fallback.
    let system_inspect = inspect
        || (!coding(&terms)
            && (any(&terms, &["show", "list"])
                || (source_file && any(&terms, &["what", "which", "where", "why", "how"]))));
    let mut names = Vec::new();
    if system_inspect
        && (any(
            &terms,
            &[
                "trace",
                "traces",
                "otlp",
                "profile",
                "profiles",
                "flamegraph",
            ],
        ) || (source_file && terms.contains("json") && any(&terms, &["span", "spans"])))
    {
        names.push("runtime_inspect");
    }
    if system_inspect && any(&terms, &["har", "openapi"]) {
        names.push("network_inspect");
    }
    if system_inspect
        && (any(&terms, &["sqlite", "database"])
            || (terms.contains("query") && terms.contains("plan")))
    {
        names.push("database");
    }
    if inspect
        && any(
            &terms,
            &[
                "document",
                "documents",
                "pdf",
                "docx",
                "pptx",
                "slides",
                "report",
            ],
        )
    {
        names.push("document");
    }
    if inspect
        && any(
            &terms,
            &[
                "data",
                "dataset",
                "datasets",
                "csv",
                "json",
                "xlsx",
                "spreadsheet",
                "workbook",
                "table",
            ],
        )
    {
        names.push("data");
    }
    if any(&terms, &["transcribe", "transcription"])
        || (inspect
            && any(
                &terms,
                &["audio", "recording", "recordings", "speech", "podcast"],
            ))
    {
        names.push("audio");
    }
    if (inspect || any(&terms, &["ocr", "annotate"]))
        && any(
            &terms,
            &[
                "image",
                "images",
                "diagram",
                "diagrams",
                "chart",
                "screenshot",
                "scan",
                "scanned",
                "ocr",
            ],
        )
    {
        names.push("visual");
    }
    if !names.is_empty()
        || (inspect
            && any(
                &terms,
                &["evidence", "citations", "citation", "sources", "quotes"],
            ))
    {
        names.extend(["evidence", "evidence_search"]);
    }
    names
}

/// Workflow-specific native tools. A task needs an actual intent, not just an incidental word in
/// a filename. The eligible catalog still decides which of these capabilities exists.
fn native(query: &str, terms: &BTreeSet<String>) -> Vec<&'static str> {
    let mut names = source_tools(query);
    // Web schemas are not a coding core. Offer them directly for web intent rather than
    // making research pay for a discovery round, or every unrelated coding step pay for them.
    // A web server, app or page is something to build, not a request to search the web.
    let web = terms.contains("web")
        && (terms.contains("search")
            || !any(
                terms,
                &[
                    "server",
                    "servers",
                    "app",
                    "application",
                    "site",
                    "page",
                    "frontend",
                ],
            ));
    if web || any(terms, &["online", "internet", "research", "browse"]) {
        names.extend(WEB_TOOLS);
    } else if any(terms, &["http", "https", "url", "website"]) {
        names.push("web_fetch");
    }
    // A page alone is too generic ("a custom 404 page", "verify it responds"): it needs a visual
    // term, while explicit UI surfaces may also be verified or inspected.
    let visual = any(
        terms,
        &["visual", "render", "rendered", "design", "redesign"],
    );
    if any(terms, &["screenshot", "playwright", "browser"])
        || (any(terms, &["ui", "frontend", "layout", "css"])
            && (visual || any(terms, &["verify", "inspect"])))
        || (terms.contains("page") && visual)
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
    // Runtime watching, not code that merely mentions servers, logs or watch modes, nor a service
    // the task asks to keep running.
    let runtime = any(
        terms,
        &[
            "crash",
            "crashes",
            "crashed",
            "errors",
            "exception",
            "exceptions",
            "live",
            "alert",
            "alerts",
            "tail",
        ],
    );
    if any(terms, &["monitor", "monitoring", "traceback"])
        || (any(terms, &["log", "server", "watch"]) && runtime)
    {
        names.push("monitor");
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
    // Version-control operations and runtime inventory, not code that merely mentions a branch or
    // a server.
    if any(
        terms,
        &[
            "git", "commit", "commits", "blame", "rebase", "stash", "staged", "unstaged",
        ],
    ) || (terms.contains("pull") && terms.contains("request"))
    {
        names.push("git");
    }
    if terms.contains("mcp") {
        names.push("mcp_servers");
    }
    names
}

fn ranked(
    query: &str,
    candidates: Vec<(String, String)>,
    priority: &impl Fn(&str) -> u8,
) -> Vec<String> {
    crate::agent::model::assistance::discover_with_priority(query, candidates, priority)
        .into_iter()
        .filter(|entry| entry.2)
        .map(|entry| entry.0)
        .collect()
}

fn select(
    query: &str,
    context: &BTreeSet<String>,
    candidates: &[(String, String)],
    priority: &impl Fn(&str) -> u8,
) -> Vec<String> {
    let terms = words(query);
    let mut selected = Vec::new();
    // Explicit tool names are strong evidence even if no workflow rule covers that specialist.
    selected.extend(
        candidates
            .iter()
            .filter(|(name, _)| {
                query.split_whitespace().any(|word| {
                    let matches = word.trim_matches(['`', ',', '.', '"']) == name;
                    // Tool names that are ordinary English words route only when explicit.
                    let common_source_name = [
                        "document",
                        "data",
                        "audio",
                        "visual",
                        "evidence",
                        "database",
                        "investigate",
                        "research",
                        "monitor",
                        "timeline",
                        "task",
                        "message",
                        "goal",
                        "reality",
                        "perception",
                        "video",
                        "workbench",
                    ]
                    .contains(&name.as_str());
                    matches
                        && (!common_source_name
                            || query.trim() == name
                            || word.starts_with('`')
                            || terms.contains("tool"))
                })
            })
            .map(|entry| entry.0.clone()),
    );
    selected.extend(
        native(query, &prose_words(query))
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
        let matches = ranked(&capability, tools.clone(), priority);
        if matches.is_empty() {
            if let Some((_, name)) = tools
                .iter()
                .filter_map(|(name, _)| entrypoint(name))
                .min_by_key(|(generic, name)| (*generic, priority(name), *name))
            {
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
            crate::agent::model::assistance::discover_with_priority(
                "library documentation",
                docs,
                priority,
            )
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
        priority,
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
        selected.extend(ranked(query, relevant, priority));
    }
    let mut seen = BTreeSet::new();
    selected.retain(|name| seen.insert(name.clone()));
    selected
}

/// A successful discovery makes same-server consumers of the discovered resource relevant.
/// Match schemas, never instructions or tool names supplied in untrusted result prose.
fn followups(
    source: &str,
    candidates: &[(String, String)],
    priority: &impl Fn(&str) -> u8,
) -> Vec<String> {
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
        priority,
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
            let matches = followups(&call.name, &self.automatic_candidates(), &|name| {
                self.tool_readiness(name).routing_priority()
            });
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

    fn refresh_task_tools(&mut self) -> bool {
        if !self.automatic_tool_routing() {
            return false;
        }
        let candidates = self.automatic_candidates();
        let mut matches = select(
            &self.tool_routing.query,
            &self.tool_routing.context,
            &candidates,
            &|name| self.tool_readiness(name).routing_priority(),
        );
        // Small contexts keep the core offer and route requested MCP capabilities plus the native
        // web pair. The pair leads, so an incidental MCP match cannot take the bounded slots first.
        if self.lean_local() || self.minimal_tools() {
            matches.retain(|name| parts(name).is_some() || WEB_TOOLS.contains(&name.as_str()));
        }
        self.add_task_tools(matches, false)
    }

    fn add_task_tools(&mut self, mut matches: Vec<String>, followup: bool) -> bool {
        let small = self.lean_local() || self.minimal_tools();
        if small {
            matches.sort_by_key(|name| !WEB_TOOLS.contains(&name.as_str()));
        }
        let before = self.tool_routing.selected.len();
        let (tool_limit, schema_limit) = if small {
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

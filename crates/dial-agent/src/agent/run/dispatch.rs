//! Tool call authorization and dispatch, code callbacks, custom tools and parallel tool groups.
use crate::config;

use crate::agent::*;

#[cfg(test)]
mod controller_tests;
#[cfg(test)]
mod image_question_tests;
pub(in crate::agent) mod result_budget;
pub(in crate::agent::run) mod search_index;
#[cfg(test)]
mod search_tests;
mod working_state;
#[cfg(test)]
mod working_state_tests;

mod background;
#[derive(Clone)]
enum DispatchMode {
    Foreground,
    Background { name: Option<String> },
}

struct DispatchAdmission<'a> {
    provenance: crate::agent::safety::approval::CallProvenance,
    arguments_changed: bool,
    mode: DispatchMode,
    read_reuse: Option<&'a crate::agent::workflow::investigation::ReadReuse>,
}

#[derive(Clone)]
struct CallbackSource<'a> {
    owner: &'a str,
    provenance: crate::agent::safety::approval::CallProvenance,
    mode: DispatchMode,
}
/// The request with its history replaced by a contract projection, or `None` when the projection
/// changed nothing. Unchanged requests stay borrowed, so the common path copies no history.
pub(in crate::agent) fn projected_request(
    request: &Chat,
    projection: Option<Vec<Value>>,
) -> Option<Chat> {
    projection.map(|messages| Chat {
        messages,
        tools: request.tools.clone(),
        temperature: request.temperature,
        max_tokens: request.max_tokens,
        seed: request.seed,
    })
}

/// Zero-based page size of the eligible-tool listing that `tool_search` serves for query `*`
/// (page 0) and `*:N` (page N). Listing only reports descriptions and status; it enables nothing.
const TOOL_LIST_PAGE: usize = 16;
/// Listing descriptions are cut to this many characters so every page stays bounded.
const TOOL_LIST_DESCRIPTION_CHARS: usize = 240;

/// `Ok(None)` for an ordinary capability query. `Ok(Some(page))` for the listing forms `*` and
/// `*:N`. A malformed or non-numeric page is refused rather than searched as a capability query.
fn listing_request(query: &str) -> Result<Option<usize>, String> {
    let query = query.trim();
    if query == "*" {
        return Ok(Some(0));
    }
    let Some(cursor) = query.strip_prefix("*:") else {
        return Ok(None);
    };
    if cursor.is_empty() || !cursor.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "invalid tool listing cursor `{query}`: use `*` for page 0 or `*:N` with a zero-based page number from next_query"
        ));
    }
    cursor
        .parse::<usize>()
        .map(Some)
        .map_err(|_| format!("invalid tool listing cursor `{query}`: page number is out of range"))
}

/// Status and the exact next action for one discovery entry. Discovery never grants permission.
/// `offered_before` says whether the tool was already in the offer before this call changed it:
/// `None` for read-only listings, which cannot tell a tool loaded earlier in this request apart
/// from one the current request already offers.
fn activation_status(loaded: bool, offered_before: Option<bool>) -> (&'static str, &'static str) {
    match (loaded, offered_before) {
        (false, _) => (
            "not_loaded",
            "call tool_search with names:[this exact name]; callable on the next request",
        ),
        (true, Some(true)) => (
            "already_offered",
            "already offered in the current request; call it by its exact name",
        ),
        (true, Some(false)) => (
            "loaded_next_request",
            "call this tool by its exact name on the next request",
        ),
        (true, None) => (
            "offered",
            "call this tool by its exact name; a tool loaded during this request becomes callable on the next request",
        ),
    }
}

/// One zero-based page of the eligible catalog. `descriptions` holds only eligible tools, so the
/// listing never reveals or activates anything outside the caller's eligibility boundary.
fn listing_output(
    page: usize,
    descriptions: &HashMap<String, String>,
    offered: &[String],
) -> Result<ToolOutput, String> {
    let mut names: Vec<&String> = descriptions.keys().collect();
    names.sort();
    let total = names.len();
    if page > 0 && page.saturating_mul(TOOL_LIST_PAGE) >= total {
        return Err(format!(
            "invalid tool listing cursor `*:{page}`: only {} page(s) of eligible tools exist; start from `*`",
            total.div_ceil(TOOL_LIST_PAGE).max(1)
        ));
    }
    let start = page * TOOL_LIST_PAGE;
    let end = (start + TOOL_LIST_PAGE).min(total);
    let tools: Vec<Value> = names[start..end]
        .iter()
        .map(|name| {
            let loaded = offered.contains(*name);
            let (status, next) = activation_status(loaded, None);
            let description: String = descriptions[*name]
                .chars()
                .take(TOOL_LIST_DESCRIPTION_CHARS)
                .collect();
            json!({"name":name,"description":description,"status":status,"enabled_next_round":loaded,"next":next})
        })
        .collect();
    let next_query = if end < total {
        json!(format!("*:{}", page + 1))
    } else {
        Value::Null
    };
    Ok(ToolOutput {
        execution: Default::default(),
        data: None,
        text: json!({"listing":true,"page":page,"page_size":TOOL_LIST_PAGE,"total":total,"next_query":next_query,"tools":tools}).to_string(),
        is_error: false,
    })
}

/// Attach advisory prerequisites without changing eligibility, activation or approval.
fn discovery_readiness(
    entry: &mut Value,
    workspace: &std::path::Path,
    config: &Config,
    registry: &crate::tools::integrations::ext::Registry,
) {
    let Some(name) = entry["name"].as_str() else {
        return;
    };
    let readiness = json!(crate::tools::surface::catalog::readiness_at(
        name, workspace, config, registry
    ));
    let operations = json!(crate::tools::surface::catalog::operation_readiness(
        name, config
    ));
    entry["readiness"] = readiness;
    entry["operation_readiness"] = operations;
}

fn listing_readiness(
    mut output: ToolOutput,
    workspace: &std::path::Path,
    config: &Config,
    registry: &crate::tools::integrations::ext::Registry,
) -> ToolOutput {
    let mut page: Value =
        serde_json::from_str(&output.text).expect("listing_output serializes a JSON object");
    if let Some(entries) = page["tools"].as_array_mut() {
        for entry in entries {
            discovery_readiness(entry, workspace, config, registry);
        }
    }
    output.text = page.to_string();
    output
}

/// Turn a successful tool's PNG `data.artifact` envelope into a compact image reference, so
/// every tool that renders pixels (browser, computer, github, derived-media `read`, `visual`,
/// `video`, `evidence`, ...) reaches a vision model through one rule instead of a name list.
/// Browser, computer and github results are replaced by the compact pointer object; for every
/// other tool the description text is the model-facing result and survives capture.
pub(in crate::agent) fn capture_output_image(
    workspace: &Workspace,
    call: &ToolCall,
    output: &mut ToolOutput,
) -> Result<Option<Value>> {
    if output.is_error
        || !output
            .data
            .as_ref()
            .is_some_and(|data| data["artifact"]["data"].is_string())
    {
        return Ok(None);
    }
    let owner = crate::tools::surface::facade::owner_view(&call.name, &call.arguments).0;
    if matches!(owner.as_str(), "browser" | "computer" | "github") {
        return crate::storage::artifact::capture_image(workspace.artifacts(), output);
    }
    let text = output.text.clone();
    let reference = crate::storage::artifact::capture_image(workspace.artifacts(), output)?;
    output.text = text;
    Ok(reference)
}

pub(in crate::agent) fn callback_image(
    workspace: &Workspace,
    call: &ToolCall,
    output: &mut ToolOutput,
    supplied: Option<Value>,
) -> Result<Option<(Value, Vec<u8>, &'static str)>> {
    if output.is_error {
        return Ok(None);
    }
    let reference = if let Some(reference) = capture_output_image(workspace, call, output)? {
        Some(reference)
    } else if call.name == "read"
        && let Some(path) = call.arguments["path"]
            .as_str()
            .filter(|path| llm::images::is_image(Path::new(path)))
    {
        Some(match supplied {
            Some(reference) => reference,
            None => {
                let absolute = workspace.resolve(path).map_err(anyhow::Error::msg)?;
                let relative = absolute.strip_prefix(workspace.root())?;
                llm::images::attachment(workspace.root(), relative)?
            }
        })
    } else {
        None
    };
    reference
        .map(|reference| {
            let (bytes, mime) = llm::images::verified_bytes(&reference)?;
            Ok((reference, bytes, mime))
        })
        .transpose()
}

/// `read IMAGE?q=QUESTION` is answered by the vision role instead of reading a file. Only image
/// paths qualify, so other `?` names stay ordinary reads.
pub(in crate::agent) fn image_question(call: &ToolCall) -> Option<(&str, &str)> {
    if call.name != "read" {
        return None;
    }
    let (path, question) = call.arguments["path"]
        .as_str()
        .and_then(llm::images::split_question)?;
    llm::images::is_image(Path::new(path)).then_some((path, question))
}

pub(in crate::agent) fn callback_image_payload(
    output: &mut ToolOutput,
    image: Option<(Value, Vec<u8>, &'static str)>,
) -> Result<()> {
    if let Some((reference, bytes, mime)) = image {
        use base64::Engine;
        let data = output.data.get_or_insert_with(|| json!({}));
        let object = data
            .as_object_mut()
            .context("image callback data must be an object")?;
        object.insert(
            "images".into(),
            json!([{
                "type":"image",
                "mimeType":mime,
                "data":base64::engine::general_purpose::STANDARD.encode(bytes),
                "reference":reference
            }]),
        );
    }
    Ok(())
}

pub(in crate::agent) fn interrupted_tool() -> ToolOutput {
    crate::tools::exec::execution::before_start()
}

/// Stdin for an operator-configured tool. With `parameters` the program receives the whole
/// arguments object after its `required` properties are checked; without them, a single `input`.
pub(in crate::agent) fn custom_tool_stdin(
    tool: &crate::config::types::ToolConfig,
    args: &Value,
) -> Result<String> {
    let Some(schema) = &tool.parameters else {
        let input = args.get("input").and_then(Value::as_str).unwrap_or("");
        return Ok(json!({"input": input}).to_string());
    };
    for key in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        ensure!(
            crate::tools::present(args, key),
            "missing required argument `{key}`"
        );
    }
    Ok(args.to_string())
}

pub(in crate::agent) fn custom_tool_output(
    tool: &crate::config::types::ToolConfig,
    args: &Value,
    cancel: &AtomicBool,
) -> ToolOutput {
    let input = match custom_tool_stdin(tool, args) {
        Ok(input) => input,
        Err(error) => {
            return ToolOutput::err(format!("{error:#}"))
                .with_effects(crate::tools::surface::outcome::Effects::None);
        }
    };
    match worker::run_tool_command(&tool.program, &tool.args, &input, tool.timeout_secs, cancel) {
        Ok(text) => ToolOutput {
            text,
            is_error: false,
            data: None,
            execution: Default::default(),
        },
        Err(error) => {
            let effects = if error.downcast_ref::<worker::ToolEffectsUnknown>().is_some() {
                crate::tools::surface::outcome::Effects::Unknown
            } else {
                crate::tools::surface::outcome::Effects::None
            };
            ToolOutput::err(format!("{error:#}")).with_effects(effects)
        }
    }
}

/// The workspace, configuration, registry, interrupt, and sinks one parallel tool batch runs against.
pub(in crate::agent) struct ParallelToolContext<'a> {
    pub(in crate::agent) workspace: &'a Workspace,
    pub(in crate::agent) config: &'a Config,
    pub(in crate::agent) ext: &'a Registry,
    pub(in crate::agent) interrupt: &'a AtomicBool,
    pub(in crate::agent) ui: &'a mut dyn Ui,
    pub(in crate::agent) pending_steering: &'a mut Vec<String>,
}

/// Run only a prevalidated consecutive group. No agent/session/UI state enters a worker.
#[cfg(test)]
pub(in crate::agent) fn parallel_tools(
    ctx: ParallelToolContext<'_>,
    calls: &[ToolCall],
    already_started: Option<&str>,
    reuse: Option<&crate::agent::workflow::investigation::ReadReuse>,
) -> Vec<(ToolOutput, f64)> {
    parallel_tools_with_mutations(ctx, calls, already_started, reuse, None, None, None)
}

/// Most entries one `tool.batch([...])` may carry; each also counts against the cell's limit.
pub(in crate::agent) const MAX_BATCH_ENTRIES: usize = 128;

/// Parse a `tool.batch` callback's `{calls:[{name, arguments}]}` payload into ordered calls with
/// ids under `owner`. Shared by the parent and worker controllers.
pub(in crate::agent) fn code_batch_calls(
    arguments: &Value,
    owner: &str,
) -> std::result::Result<Vec<ToolCall>, String> {
    let Some(entries) = arguments["calls"].as_array() else {
        return Err("tool.batch expects an array of {name, args} entries".into());
    };
    if entries.len() > MAX_BATCH_ENTRIES {
        return Err(format!(
            "tool.batch accepts at most {MAX_BATCH_ENTRIES} entries"
        ));
    }
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let (Some(name), Some(arguments)) = (
                entry["name"].as_str().filter(|name| !name.is_empty()),
                entry
                    .get("arguments")
                    .filter(|arguments| arguments.is_object()),
            ) else {
                return Err(format!(
                    "tool.batch entry {index} needs a tool name and an arguments object"
                ));
            };
            Ok(ToolCall {
                id: format!("{owner}.batch{index}"),
                name: name.to_owned(),
                arguments: arguments.clone(),
                malformed_arguments: false,
            })
        })
        .collect()
}

/// The ordered `tool.batch` receipt: one `{text, data, is_error}` per entry. The batch itself
/// succeeds; a failed entry is reported in its own slot rather than raised.
pub(in crate::agent) fn code_batch_output(outputs: Vec<ToolOutput>) -> ToolOutput {
    let failed = outputs.iter().filter(|output| output.is_error).count();
    let changed: Vec<_> = outputs
        .iter()
        .filter(|output| !output.execution.changed_paths.is_empty())
        .collect();
    let mut execution = crate::runtime::execution::Execution {
        changed_paths: changed
            .iter()
            .flat_map(|output| output.execution.changed_paths.iter().cloned())
            .collect(),
        mutation_settled: !changed.is_empty()
            && changed
                .iter()
                .all(|output| output.execution.mutation_settled),
        ..Default::default()
    };
    execution.changed_paths.sort();
    execution.changed_paths.dedup();
    let unknown = outputs
        .iter()
        .any(|output| output.execution.effects == tools::surface::outcome::Effects::Unknown);
    ToolOutput {
        execution,
        text: format!(
            "tool.batch settled {} callback(s), {failed} failed",
            outputs.len()
        ),
        data: Some(Value::Array(
            outputs
                .into_iter()
                .enumerate()
                .map(|(index, output)| {
                    let status = tools::ToolStatus::from_output(&output);
                    json!({"index":index,"text":output.text,"data":output.data,
                        "is_error":output.is_error,"status":status.as_str(),
                        "effects":output.execution.effects,
                        "changed_paths":output.execution.changed_paths,
                        "process":output.execution.process,"output":output.execution.output})
                })
                .collect(),
        )),
        is_error: false,
    }
    .with_effects(if unknown {
        tools::surface::outcome::Effects::Unknown
    } else {
        tools::surface::outcome::Effects::Known
    })
}

pub(in crate::agent) fn parallel_tools_with_reads(
    ctx: ParallelToolContext<'_>,
    calls: &[ToolCall],
    already_started: Option<&str>,
    reuse: Option<&crate::agent::workflow::investigation::ReadReuse>,
    streamed: Option<&mut crate::agent::run::streaming_dispatch::StreamingDispatch>,
) -> Vec<(ToolOutput, f64)> {
    parallel_tools_with_mutations(ctx, calls, already_started, reuse, None, None, streamed)
}

type GroupMutationController<'a> =
    &'a mut dyn FnMut(&ToolCall, &str, &[PathBuf], bool) -> std::result::Result<(), String>;

pub(in crate::agent) fn parallel_tools_with_mutations(
    ctx: ParallelToolContext<'_>,
    calls: &[ToolCall],
    already_started: Option<&str>,
    read_reuse: Option<&crate::agent::workflow::investigation::ReadReuse>,
    mut mutation_controller: Option<GroupMutationController<'_>>,
    turn_cancel: Option<&crate::runtime::cancellation::TurnCancellation>,
    mut streamed: Option<&mut crate::agent::run::streaming_dispatch::StreamingDispatch>,
) -> Vec<(ToolOutput, f64)> {
    let ParallelToolContext {
        workspace,
        config,
        ext,
        interrupt,
        ui,
        pending_steering,
    } = ctx;
    let signals = crate::tools::exec::signals::CallSignals::new();
    thread::scope(|scope| {
        let (completed, completions) = mpsc::channel();
        type Mutation = (
            usize,
            String,
            Vec<PathBuf>,
            bool,
            mpsc::SyncSender<std::result::Result<(), String>>,
        );
        let (mutations, requests) = mpsc::channel::<Mutation>();
        let mutating = mutation_controller.is_some();
        let mut unique: HashMap<(&str, String), usize> = HashMap::new();
        let mut sources = Vec::with_capacity(calls.len());
        let mut remaining = Vec::new();
        let mut first_calls: Vec<usize> = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if already_started != Some(call.id.as_str()) {
                ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
            }
            let key = (
                call.name.as_str(),
                crate::agent::safety::guard::normalized_arguments(call).to_string(),
            );
            if let Some(&source) = unique.get(&key)
                && !mutating
                && !["pre_tool_use", "post_tool_use"].iter().any(|event| {
                    crate::agent::safety::hooks::wanted_for_tool(&config.hooks, event, &call.name)
                })
            {
                sources.push(source);
                remaining[source] += 1;
                ui.diagnostic(&format!(
                    "Batch reuse: {} shares the result of {}; no duplicate inspection runs.",
                    call.id, calls[first_calls[source]].id
                ));
                continue;
            }
            let source = first_calls.len();
            unique.insert(key, source);
            sources.push(source);
            remaining.push(1usize);
            first_calls.push(index);
            let completed = completed.clone();
            let mutations = mutations.clone();
            let mutation_scope = turn_cancel.map(|turn| turn.mutation());
            if let Some(error) = ui.execution_error() {
                let output = ToolOutput::err(error)
                    .with_status(tools::ToolStatus::Blocked)
                    .with_effects(crate::tools::surface::outcome::Effects::None);
                let _ = completed.send((source, (output, 0.0), Vec::new(), None));
                continue;
            }
            let early = streamed.as_mut().and_then(|stream| stream.extract(call));
            let signals = signals.clone();
            scope.spawn(move || {
                if let Some(outcome) = early.and_then(|early| early.receive()) {
                    let _ = completed.send((source,outcome,Vec::new(),None));
                    return;
                }
                let started = Instant::now();
                let mut notices = Vec::new();
                let mut policy_reason = None;
                let output = crate::tools::exec::execution::run(interrupt, || crate::tools::exec::signals::scope(&signals, || {
                    if interrupt.load(Ordering::SeqCst) {
                        return interrupted_tool();
                    }
                    match crate::agent::safety::hooks::pre_tool_with(&config.hooks, workspace.root(), &call.name, &call.arguments, interrupt, &mut |notice| notices.push(notice.to_owned())) {
                        Err(error) => {
                            policy_reason = Some(crate::agent::safety::approval::PolicyReason::HookVeto);
                            return ToolOutput::err(error).with_status(tools::ToolStatus::Blocked);
                        }
                        Ok(Some(args)) if args != call.arguments => {
                            policy_reason = Some(crate::agent::safety::approval::PolicyReason::HookVeto);
                            return ToolOutput::err("pre_tool_use changed a scheduled call; retry the revised arguments through the execution controller").with_status(tools::ToolStatus::Blocked);
                        }
                        _ => {}
                    }
                    let authorize = |tool: &str, paths: &[PathBuf], unbounded: bool| {
                        if interrupt.load(Ordering::SeqCst) { return Err("native group cancelled before authorization".into()); }
                        let (sender,receiver) = mpsc::sync_channel(1);
                        mutations.send((index,tool.to_owned(),paths.to_vec(),unbounded,sender))
                            .map_err(|_| "native group owner disconnected".to_owned())?;
                        loop { match receiver.recv_timeout(Duration::from_millis(80)) {
                            Ok(result) => return result,
                            Err(RecvTimeoutError::Timeout) if !interrupt.load(Ordering::SeqCst) => {},
                            _ => return Err("native group authorization cancelled or disconnected".into()),
                        }}
                    };
                    let mut read = |args: &Value| {
                        let ctx = ToolContext { workspace, config, mutation_authorizer: mutating.then_some(&authorize as crate::tools::integrations::ext::MutationAuthorizer<'_>), confirm: None };
                        let execute = || if mutating {
                            crate::tools::exec::execution::native(&ctx,ext,&call.name,args,&|_|{},interrupt)
                        } else { crate::tools::exec::execution::native_read(&ctx,ext,&call.name,args,&|_|{},interrupt) };
                        let output = match &mutation_scope {
                            Some(scope) => crate::tools::exec::execution::mutation_scope(scope,execute),
                            None => execute(),
                        };
                        output.unwrap_or_else(|| ToolOutput::err("parallel tool unavailable"))
                    };
                    let mut output = if call.name == "read"
                        && !["pre_tool_use", "post_tool_use"].iter().any(|event| {
                            crate::agent::safety::hooks::wanted_for_tool(&config.hooks, event, &call.name)
                        })
                        && let Some(reuse) = read_reuse
                    {
                        reuse.execute(workspace, &call.arguments, &mut read)
                    } else {
                        read(&call.arguments)
                    };
                    crate::agent::safety::hooks::post_tool_with(&config.hooks, workspace.root(), &call.name, &call.arguments, &mut output, interrupt, &mut |notice| notices.push(notice.to_owned()));
                    output
                }));
                let _ = completed.send((source, (output, started.elapsed().as_secs_f64()), notices, policy_reason));
            });
        }
        drop(completed);
        let mut results: Vec<Option<(ToolOutput, f64)>> =
            (0..first_calls.len()).map(|_| None).collect();
        let mut outstanding = results.len();
        let mut input_signalled = false;
        while outstanding > 0 {
            while let Ok((index, tool, paths, unbounded, reply)) = requests.try_recv() {
                let result = match mutation_controller.as_deref_mut() {
                    Some(authorize) => authorize(&calls[index], &tool, &paths, unbounded),
                    None => Err("parallel reads have no mutation authority".into()),
                };
                let _ = reply.send(result);
            }
            let steering = ui.take_steering();
            if !steering.is_empty() {
                signals.request_steering();
                signals.interrupt_waits();
                pending_steering.extend(steering);
                for call in calls {
                    if let Some(tool) = ext
                        .get(&call.name)
                        .filter(|tool| tool.interruptible(&call.arguments))
                    {
                        tool.cancel_operation();
                    }
                }
            } else if !input_signalled && ui.pending_input() {
                // Queued input is routed at the step boundary; parallel waits end so it is prompt.
                input_signalled = true;
                signals.request_steering();
                signals.interrupt_waits();
            }
            if interrupt.load(Ordering::SeqCst) {
                signals.interrupt_waits();
                for call in calls {
                    if let Some(tool) = ext.get(&call.name) {
                        tool.cancel_operation();
                    }
                }
            }
            let (source, result, notices, policy_reason) = match completions
                .recv_timeout(Duration::from_millis(if mutating { 5 } else { 80 }))
            {
                Ok(completion) => completion,
                Err(RecvTimeoutError::Timeout) => {
                    ui.heartbeat();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    for (source, result) in results.iter_mut().enumerate() {
                        if result.is_none() {
                            let output = ToolOutput::err("tool worker disconnected");
                            for (index, call) in calls.iter().enumerate() {
                                if sources[index] == source {
                                    ui.tool_result_with_id(&call.id, &call.name, &output);
                                }
                            }
                            *result = Some((output, 0.0));
                        }
                    }
                    break;
                }
            };
            if let Some(reason) = policy_reason {
                crate::agent::safety::approval::explain(ui, &calls[first_calls[source]], reason);
            }
            for notice in notices {
                ui.notice(&notice);
            }
            // Visible completion is independent of the ordered provider-history settlement.
            // StateUi journals each result before forwarding it to any surface.
            for (index, call) in calls.iter().enumerate() {
                if sources[index] == source {
                    ui.tool_timing(
                        &call.id,
                        &call.name,
                        if index == first_calls[source] {
                            result.1
                        } else {
                            0.0
                        },
                    );
                    ui.tool_result_with_id(&call.id, &call.name, &result.0);
                }
            }
            results[source] = Some(result);
            outstanding -= 1;
        }
        sources
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                remaining[source] -= 1;
                let (output, seconds) = if remaining[source] == 0 {
                    results[source].take().expect("unconsumed batch result")
                } else {
                    results[source]
                        .as_ref()
                        .expect("shared batch result")
                        .clone()
                };
                (
                    output,
                    if index == first_calls[source] {
                        seconds
                    } else {
                        0.0
                    },
                )
            })
            .collect()
    })
}

impl<'a> Agent<'a> {
    pub(in crate::agent) fn authorize_call(
        &mut self,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> std::result::Result<(), crate::agent::safety::approval::Denial> {
        self.authorize_call_from(
            call,
            crate::agent::safety::approval::CallProvenance::Structured,
            ui,
        )
    }

    pub(in crate::agent) fn authorize_call_from(
        &mut self,
        call: &ToolCall,
        provenance: crate::agent::safety::approval::CallProvenance,
        ui: &mut dyn Ui,
    ) -> std::result::Result<(), crate::agent::safety::approval::Denial> {
        let once = self
            .pending_batch_approvals
            .iter()
            .position(|(prepared, _)| {
                prepared.id == call.id
                    && prepared.name == call.name
                    && prepared.arguments == call.arguments
                    && prepared.malformed_arguments == call.malformed_arguments
            })
            .map(|index| self.pending_batch_approvals.remove(index).1);
        let local_memory_write =
            crate::storage::memory_backend::call_action(&call.name, &call.arguments)
                == Some("learn")
                && self.memory_runtime.as_ref().is_some_and(|memory| {
                    memory.id() == crate::config::memory_backend::MemoryBackendId::Local
                });
        let result = if crate::agent::safety::approval::denied_by_rule(
            &self.always,
            call,
            self.workspace.root(),
        ) {
            crate::agent::safety::approval::explain_rule(
                ui,
                call,
                self.workspace.root(),
                &self.always,
                crate::agent::safety::approval::Effect::Deny,
            );
            Err(crate::agent::safety::approval::Denial::Rule)
        } else if self.plan_mode
            && !matches!(call.name.as_str(), "request_directory" | "exit_plan")
            && (local_memory_write
                || !crate::agent::safety::approval::is_read_only(
                    &self.ext,
                    &call.name,
                    &call.arguments,
                ))
        {
            crate::agent::safety::approval::explain(
                ui,
                call,
                crate::agent::safety::approval::PolicyReason::PlanMode,
            );
            Err(crate::agent::safety::approval::Denial::ReadOnly)
        } else if provenance == crate::agent::safety::approval::CallProvenance::RecoveredText {
            crate::agent::safety::approval::authorize_recovered_call(
                &crate::agent::safety::approval::WorkerPolicy {
                    approval: self.approval,
                    rules: &mut self.always,
                    root: self.workspace.root(),
                    read_only: self.plan_mode,
                    cancel: Some(self.interrupt.as_ref()),
                    interrupt: None,
                },
                call,
                &describe(call),
                ui,
            )
        } else if let Some(allowed) = once {
            crate::agent::safety::approval::authorize_once(
                self.approval,
                &mut self.always,
                self.workspace.root(),
                call,
                ui,
                self.interrupt.as_ref(),
                allowed,
            )
            .map(|_| ())
        } else if local_memory_write
            || crate::agent::safety::approval::needs_approval(&self.ext, call)
        {
            crate::agent::safety::approval::authorize_with_cancel(
                self.approval,
                &mut self.always,
                self.workspace.root(),
                call,
                &describe(call),
                ui,
                self.interrupt.as_ref(),
            )
            .map(|persist| {
                if persist && let Err(error) = self.persist_permissions() {
                    ui.notice(&format!(
                        "The rule applies to this session but could not be saved: {error:#}"
                    ));
                }
            })
        } else {
            crate::agent::safety::approval::explain(
                ui,
                call,
                crate::agent::safety::approval::PolicyReason::SafeCall,
            );
            Ok(())
        };
        // The denial text is model-facing and travels in the tool result. The user already saw
        // the policy explanation, or made the decision themselves; no duplicate notice here.
        result
    }

    /// Whether authorizing `call` now would stop for a human approval prompt. A dry run of
    /// [`authorize_call_from`](Self::authorize_call_from)'s policy against copied rules and a
    /// silent surface; it grants nothing and changes no state. Only a structured call that reaches
    /// the interactive prompt counts: denials, plan mode, rules, Auto and once-approvals do not.
    pub(in crate::agent) fn approval_prompt_pending(
        &self,
        call: &ToolCall,
        provenance: crate::agent::safety::approval::CallProvenance,
    ) -> bool {
        use crate::agent::safety::approval;
        #[derive(Default)]
        struct Probe(bool);
        impl Ui for Probe {
            fn approve_call(
                &mut self,
                _: &str,
                _: &str,
                _: &str,
                _: Option<&AtomicBool>,
            ) -> Decision {
                self.0 = true;
                Decision::Deny
            }
        }
        if provenance != approval::CallProvenance::Structured
            || self.plan_mode
            || approval::denied_by_rule(&self.always, call, self.workspace.root())
            || self.pending_batch_approvals.iter().any(|(prepared, _)| {
                prepared.id == call.id
                    && prepared.name == call.name
                    && prepared.arguments == call.arguments
            })
            || !approval::needs_approval(&self.ext, call)
        {
            return false;
        }
        let mut probe = Probe::default();
        let _ = approval::authorize_with_cancel(
            self.approval,
            &mut self.always.clone(),
            self.workspace.root(),
            call,
            &describe(call),
            &mut probe,
            &AtomicBool::new(false),
        );
        probe.0
    }

    /// Operator hooks apply to live agents only; evaluation runs must stay hermetic.
    pub(in crate::agent) fn tool_hooks(&self) -> &[crate::config::types::HookConfig] {
        if self.hermetic {
            &[]
        } else {
            &self.config.hooks
        }
    }

    /// `before_provider_request` for one model call made by this agent. Hook notices reach `ui`;
    /// `None` means send the borrowed request unchanged.
    pub(in crate::agent) fn provider_request_with(
        &self,
        request: &Chat,
        ui: &mut dyn Ui,
    ) -> Result<Option<Chat>> {
        let projected = projected_request(
            request,
            // Recorded contracts are checked against the callable set only: a cache-stable
            // surface (`llm::tool_offer`) also sends tools this request does not offer.
            crate::tools::surface::contracts::project_history(
                &request.messages,
                &crate::llm::tool_offer::callable(&request.tools),
                &self.ext,
            )?,
        );
        let mut notices = Vec::new();
        let rewritten = crate::agent::safety::hooks::provider_request(
            self.tool_hooks(),
            self.workspace.root(),
            projected.as_ref().unwrap_or(request),
            self.interrupt.as_ref(),
            &mut |notice| notices.push(notice.to_owned()),
        )?;
        for notice in &notices {
            ui.notice(notice);
        }
        Ok(rewritten.or(projected))
    }

    pub(in crate::agent) fn dispatch(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        self.dispatch_from(
            backend,
            call,
            crate::agent::safety::approval::CallProvenance::Structured,
            ui,
        )
    }

    pub(in crate::agent) fn dispatch_from(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        provenance: crate::agent::safety::approval::CallProvenance,
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        self.dispatch_from_reusing(backend, call, provenance, ui, None)
    }

    pub(in crate::agent) fn dispatch_from_reusing(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        provenance: crate::agent::safety::approval::CallProvenance,
        ui: &mut dyn Ui,
        read_reuse: Option<&crate::agent::workflow::investigation::ReadReuse>,
    ) -> ToolOutput {
        let output = self.dispatch_call_mode(
            backend,
            call,
            provenance,
            ui,
            DispatchMode::Foreground,
            read_reuse,
        );
        modes::lifecycle::Lifecycle::dispatched(self, call, &output);
        output
    }

    /// `read path?q=question` on an image asks the `vision` role and returns only its answer. The
    /// pixels travel in that one-shot request and never enter this conversation.
    fn answer_image_question(&self, path: &str, question: &str) -> ToolOutput {
        match self.image_answer(path, question) {
            Ok(answer) => ToolOutput::ok(answer),
            Err(error) => ToolOutput::err(format!("{error:#}")),
        }
    }

    fn image_answer(&self, path: &str, question: &str) -> Result<String> {
        const MAX_ANSWER_TOKENS: u32 = 1024;
        ensure!(
            llm::images::is_image(Path::new(path)),
            "?q= answers questions about image files; {path} is not one"
        );
        ensure!(
            self.config.role_selection("vision").is_some(),
            "no vision model is configured; assign one to the `vision` role to answer ?q= questions"
        );
        let absolute = self.workspace.resolve(path).map_err(anyhow::Error::msg)?;
        let relative = absolute.strip_prefix(self.workspace.root())?;
        let reference = llm::images::attachment(self.workspace.root(), relative)?;
        let image = llm::images::image_part(&reference)?;
        let vision = self.config.for_role("vision");
        let request = Chat {
            messages: vec![
                json!({"role": "user", "content": [{"type": "text", "text": question}, image]}),
            ],
            tools: Vec::new(),
            temperature: 0.0,
            max_tokens: vision
                .active_model_profile()
                .and_then(|profile| profile.max_tokens)
                .unwrap_or(MAX_ANSWER_TOKENS),
            seed: None,
        };
        let mut backend = make_backend(&vision)?;
        let result = backend.chat_with_control(
            None,
            &request,
            vision.worker_timeout_secs,
            None,
            &mut |_| {},
            self.interrupt.clone(),
        );
        backend.release();
        Ok(result?.text)
    }

    fn dispatch_call_mode(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        provenance: crate::agent::safety::approval::CallProvenance,
        ui: &mut dyn Ui,
        mode: DispatchMode,
        read_reuse: Option<&crate::agent::workflow::investigation::ReadReuse>,
    ) -> ToolOutput {
        if matches!(mode, DispatchMode::Background { .. })
            && crate::agent::safety::hooks::wanted_for_tool(
                self.tool_hooks(),
                "post_tool_use",
                &call.name,
            )
        {
            return ToolOutput::err("background tool calls are unavailable with post_tool_use hooks; run the tool in the foreground so its completed result reaches the hook")
                .with_effects(crate::tools::surface::outcome::Effects::None);
        }
        let mut expanded = call.clone();
        if let Err(error) =
            crate::agent::window::file_handles::expand(&self.workspace, &mut expanded)
        {
            return ToolOutput::err(format!("File reference refused: {error}"))
                .with_effects(crate::tools::surface::outcome::Effects::None);
        }
        let call = &expanded;
        if self
            .ext
            .get(&call.name)
            .is_some_and(|tool| tool.supports_tool_dispatch())
        {
            self.callback_question_results.clear();
        }
        if call.name == "finish" {
            self.step_checks.finish_accepted = false;
        }
        let mut rewritten = None;
        if call.name != "exit_plan"
            && crate::agent::safety::hooks::wanted(self.tool_hooks(), "pre_tool_use")
        {
            let root = self.workspace.root().to_path_buf();
            let configured_hooks = self.tool_hooks();
            let interrupt = self.interrupt.as_ref();
            let mut notices = Vec::new();
            let (result, context) = crate::agent::safety::hooks::with_heartbeat(ui, || {
                crate::agent::safety::hooks::pre_tool_noted(
                    configured_hooks,
                    &root,
                    &call.name,
                    &call.arguments,
                    interrupt,
                    &mut |notice| notices.push(notice.to_owned()),
                )
            });
            notices.iter().for_each(|notice| {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Hook,
                    notice,
                ))
            });
            self.queue_hook_context(context);
            match result {
                Err(reason) => {
                    crate::agent::safety::approval::explain(
                        ui,
                        call,
                        crate::agent::safety::approval::PolicyReason::HookVeto,
                    );
                    if let Err(error) = self.record(
                        self.cur_step,
                        "tool_denied",
                        json!({"name":call.name,"hook":true}),
                    ) {
                        ui.notice(&format!("tool denial was not journaled: {error:#}"));
                    }
                    return ToolOutput {
                        execution: Default::default(),
                        data: None,
                        text: reason,
                        is_error: true,
                    };
                }
                Ok(arguments) => rewritten = arguments,
            }
        }
        let arguments_changed = rewritten
            .as_ref()
            .is_some_and(|arguments| arguments != &call.arguments);
        // A rewritten call still runs through schemas and approvals inside `dispatch_inner`.
        let effective = match rewritten {
            Some(arguments) => ToolCall {
                arguments,
                ..call.clone()
            },
            None => call.clone(),
        };
        let call = &effective;
        let mut approval_denied = false;
        let mut out = self.dispatch_inner(
            backend,
            call,
            ui,
            &mut approval_denied,
            DispatchAdmission {
                provenance,
                arguments_changed,
                mode,
                read_reuse,
            },
        );
        if call.name != "exit_plan"
            && !approval_denied
            && crate::agent::safety::hooks::wanted(self.tool_hooks(), "post_tool_use")
        {
            let root = self.workspace.root().to_path_buf();
            let configured_hooks = self.tool_hooks();
            let interrupt = self.interrupt.as_ref();
            let mut notices = Vec::new();
            let context = crate::agent::safety::hooks::with_heartbeat(ui, || {
                crate::agent::safety::hooks::post_tool_with(
                    configured_hooks,
                    &root,
                    &call.name,
                    &call.arguments,
                    &mut out,
                    interrupt,
                    &mut |notice| notices.push(notice.to_owned()),
                )
            });
            notices.iter().for_each(|notice| {
                ui.event(&crate::agent::ui::ui_event::UiEvent::new(
                    crate::agent::ui::ui_event::EventKind::Hook,
                    notice,
                ))
            });
            self.queue_hook_context(context);
        }
        out.execution
            .invocation
            .get_or_insert_with(crate::runtime::execution::Invocation::fresh);
        if let Some(advice) = self.settle_tool_mutation(call, &mut out) {
            crate::tools::surface::outcome::append_text(&mut out, &format!("\n\n{advice}"));
        }
        self.post_mutation_verification(call, &mut out, ui);
        out
    }

    pub(in crate::agent) fn post_mutation_verification(
        &mut self,
        call: &ToolCall,
        out: &mut ToolOutput,
        ui: &mut dyn Ui,
    ) {
        if matches!(call.name.as_str(), "edit" | "write") {
            use crate::agent::verify::verify_tool::VerifyStep;
            match self.verify_batch.mutation_finished(call, !out.is_error) {
                VerifyStep::Nothing => {}
                VerifyStep::Deferred => crate::tools::surface::outcome::append_text(
                    out,
                    "\n\n[verify deferred: runs once after this response's last edit]",
                ),
                VerifyStep::Skipped(count) => crate::tools::surface::outcome::append_text(
                    out,
                    &format!(
                        "\n\n[verify not run: this {} failed, so {count} requested test target(s) did not run]",
                        call.name
                    ),
                ),
                VerifyStep::Run(targets) => {
                    if let Some(advice) = self.settle_tool_mutation(call, out) {
                        crate::tools::surface::outcome::append_text(out, &format!("\n\n{advice}"));
                    }
                    let verify = self.verify_tool(&json!({"targets": targets}), ui);
                    crate::agent::verify::verify_tool::attach_post_mutation_verify(
                        out, &call.name, verify,
                    );
                }
            }
        }
    }

    fn dispatch_inner(
        &mut self,
        backend: &mut Backend,
        call: &ToolCall,
        ui: &mut dyn Ui,
        approval_denied: &mut bool,
        admission: DispatchAdmission<'_>,
    ) -> ToolOutput {
        let DispatchAdmission {
            provenance,
            arguments_changed,
            mode,
            read_reuse,
        } = admission;
        let mut expanded = call.clone();
        if let Err(error) =
            crate::agent::window::file_handles::expand(&self.workspace, &mut expanded)
        {
            return ToolOutput::err(format!("File reference refused: {error}"))
                .with_effects(crate::tools::surface::outcome::Effects::None);
        }
        let arguments_changed = arguments_changed || expanded.arguments != call.arguments;
        let policy_call = expanded.clone();
        // Controller facades keep the accepted/journaled call identity, but all execution and
        // authorization below use the existing operation owner and its original arguments.
        if matches!(
            expanded.name.as_str(),
            "tool_search" | "skill" | "checkpoint" | "task" | "ask"
        ) || (expanded.name == "memory" && expanded.arguments["store"] == "codebase")
        {
            let (owner, arguments) =
                crate::tools::surface::facade::owner_view(&expanded.name, &expanded.arguments);
            expanded.name = owner;
            expanded.arguments = arguments;
        }
        let controller_routed =
            expanded.name != policy_call.name || expanded.arguments != policy_call.arguments;
        let call = &expanded;
        if call.name == "bash"
            && let Some(command) = call.arguments["command"].as_str()
        {
            let key = format!(
                "{}\0{command}",
                call.arguments["cwd"].as_str().unwrap_or_default()
            );
            if self.step_checks.slow_checks.get(&key) == Some(&self.step_checks.mutations) {
                ui.notice("Repeating a slow check with no intervening native source edit observed. Prefer the retained output or a scoped check; external input changes may still require a rerun.");
            }
        }
        let denied = |why: &str| {
            ToolOutput {
                execution: Default::default(),
                data: None,
                text: why.to_owned(),
                is_error: true,
            }
            .with_status(tools::ToolStatus::Blocked)
            .with_effects(crate::tools::surface::outcome::Effects::None)
        };
        if matches!(
            expanded.name.as_str(),
            "tool_search"
                | "tool_load"
                | "skill"
                | "skill_manage"
                | "checkpoint"
                | "rewind"
                | "task"
                | "side_request"
                | "ask"
                | "request_directory"
        ) && !self.controller_operation_available(&expanded.name)
        {
            return denied(
                "this controller operation is unavailable under the current mode or tool allowlist",
            );
        }
        if expanded.name == "codebase_memory"
            && (!self.ext.contains("codebase_memory")
                || !(self.modes.launch.allows_tool("codebase_memory")
                    || self.modes.launch.allows_tool("memory"))
                || self
                    .config
                    .active_model_profile()
                    .and_then(|profile| profile.tools.as_ref())
                    .is_some_and(|allowed| {
                        !allowed
                            .iter()
                            .any(|name| matches!(name.as_str(), "memory" | "codebase_memory"))
                    }))
        {
            return denied("saved codebase memory is unavailable under the current tool allowlist");
        }
        if self.edit_preview_intercepted {
            return denied(
                "This callback was not executed: an earlier authorized edit preview failed in this assistant batch.",
            );
        }
        // Unchanged calls already passed the request contract at admission. Only a
        // hook rewrite invalidates that evidence; authority is always checked below.
        if arguments_changed || controller_routed {
            let names = std::slice::from_ref(&call.name);
            let schemas = match self.schemas(names) {
                Ok(schemas) => schemas,
                Err(error) => return denied(&format!("resolving tool schema: {error:#}")),
            };
            if let Some((_, why)) = call_form_error(call, &schemas, names) {
                return denied(&format!("invalid tool call: {why}"));
            }
        }
        if matches!(mode, DispatchMode::Background { .. })
            && !self.ext.get(&call.name).is_some_and(|tool| {
                tool.supports_background(&call.arguments)
                    && !tool.may_modify_workspace(&call.arguments)
                    && !tool.supports_tool_dispatch()
            })
        {
            return denied(
                "this tool operation does not support background execution; run it in the foreground",
            );
        }
        if matches!(call.name.as_str(), "edit" | "write")
            && call.arguments["verify"]
                .as_array()
                .is_some_and(|entries| !entries.is_empty())
            && !self.config.verification.enabled
        {
            return denied(&format!(
                "{} verify requires verification.enabled; no mutation was executed",
                call.name
            ));
        }
        if self.repro_first_fires(call, crate::config::types::ReproFirst::Gate) {
            return denied(crate::agent::verify::repro_first::GATE);
        }
        // These control tools have their own execution branch; recovered text must still
        // pass the same one-shot human boundary before entering it.
        if provenance == crate::agent::safety::approval::CallProvenance::RecoveredText
            && matches!(call.name.as_str(), "request_directory" | "exit_plan")
            && let Err(denial) = self.authorize_call_from(&policy_call, provenance, ui)
        {
            *approval_denied = true;
            return denied(denial.message_in(self.plan_mode));
        }
        if crate::agent::safety::approval::denied_by_rule(
            &self.always,
            &policy_call,
            self.workspace.root(),
        ) {
            *approval_denied = true;
            return denied("denied by permission rule");
        }
        if call.name == "request_directory" {
            return self.request_directory(call, ui);
        }
        if call.name == "exit_plan" && self.plan_mode {
            let Some(plan) = call.arguments["plan"]
                .as_str()
                .filter(|p| !p.trim().is_empty())
            else {
                return denied("exit_plan requires a nonempty plan");
            };
            self.latest_plan = Some(plan.to_owned());
            if provenance == crate::agent::safety::approval::CallProvenance::Structured
                && self.plan_yolo_auto_approves()
            {
                return match self.plan_yolo_approve(plan, ui) {
                    Ok(()) => ToolOutput { execution: Default::default(),  data: None,
                        text: "Plan approved; implementation is now permitted subject to normal tool approvals.".into(), is_error: false },
                    Err(error) => denied(&format!("Plan-yolo approval failed: {error:#}")),
                };
            }
            match ui.approve("exit_plan", plan) {
                Decision::Allow | Decision::AllowAlways | Decision::AllowProject => {
                    self.accept_plan(plan, ui);
                    return ToolOutput { execution: Default::default(),  data: None,
                        text: "Plan approved; implementation is now permitted subject to normal tool approvals.".into(), is_error: false };
                }
                Decision::Deny => {
                    // Ctrl+C at the plan prompt stops the turn: no second feedback prompt.
                    let feedback = if self.interrupt.load(Ordering::SeqCst) {
                        None
                    } else {
                        ui.approval_reason()
                    };
                    return denied(&format!(
                        "Plan rejected; remain in plan mode. {}",
                        feedback.unwrap_or_else(|| "Ask the user what to revise.".into())
                    ));
                }
            }
        }
        if let Err(denial) = self.authorize_call_from(&policy_call, provenance, ui) {
            *approval_denied = true;
            if let Err(error) = self.record(self.cur_step, "tool_denied", json!({"name":call.name}))
            {
                ui.notice(&format!("tool denial was not journaled: {error:#}"));
            }
            return denied(denial.message_in(self.plan_mode));
        }
        if self.interrupt.load(Ordering::SeqCst) {
            return interrupted_tool();
        }
        if let DispatchMode::Background { name } = mode {
            return self.launch_background_tool(call, name);
        }
        if call.name == "bg" && call.arguments.get("call").is_some() {
            return self.start_background_tool(backend, call, provenance, ui);
        }
        if let Some((path, question)) = image_question(call) {
            return self.answer_image_question(path, question);
        }
        if let Some(preview) = self.authorized_edit_preview(call) {
            self.edit_preview_intercepted = true;
            return preview;
        }
        let args = &call.arguments;
        let name = call.name.as_str();
        if crate::agent::workflow::exploration::TOOL_NAMES.contains(&name) {
            return self.dispatch_exploration(call);
        }
        if self.exploration.busy() && matches!(name, "new_context" | "finish") {
            return denied(
                "An investigation checkpoint is active or pending. Complete rewind before new_context or finish; actual effects, notebook and unfinished work remain intact.",
            );
        }
        if name == "ask" {
            let request = match crate::agent::workflow::questions::prepare(
                args,
                self.config,
                self.plan_mode,
                self.session.id(),
            ) {
                Ok(request) => request,
                Err(error) => return ToolOutput::err(error),
            };
            match self.completion_decision(TurnOutcome::NeedsInput, &args.to_string(), ui) {
                crate::agent::verify::completion::Decision::Continue(note)
                | crate::agent::verify::completion::Decision::Incomplete(note) => {
                    return ToolOutput::err(note);
                }
                crate::agent::verify::completion::Decision::Accept(_) => {}
            }
            let outcome = ui.ask_questions(&request, self.interrupt.as_ref());
            return crate::agent::workflow::questions::output(
                &request,
                outcome,
                self.interrupt.as_ref(),
            );
        }
        if crate::agent::window::context_notes::TOOL_NAMES.contains(&name) {
            let owner = self.session.id().to_owned();
            let enabled = self.config.compaction.experimental_context_management;
            if name == "context_notes" {
                let output = crate::agent::window::context_notes::execute_notes(
                    &mut self.session,
                    args,
                    &owner,
                    enabled,
                    &self.interrupt,
                );
                if let Some(memory) = &self.memory_runtime {
                    memory.resume(memory_authority(&self.session, 0));
                }
                return output;
            }
            return match crate::agent::window::context_notes::request_new_context(
                &self.session,
                args,
                &owner,
                &call.id,
                enabled,
                &self.interrupt,
            ) {
                Ok((output, request)) => {
                    self.pending_context_rollover = Some(request);
                    output
                }
                Err(error) => ToolOutput::err(error.to_string()),
            };
        }
        if name == "session_search" && args.get("view").is_some() {
            let selection = self.auth_config.selection();
            return crate::agent::compact::snapcompact::views::call(
                &self.session.messages(),
                args,
                crate::agent::compact::snapcompact::views::ViewContext {
                    policy: &self.config.compaction,
                    identity: crate::config::compaction::CompactionIdentity {
                        provider: &selection.provider,
                        model: &selection.model,
                        agent: None,
                    },
                    window: self.budget_window(),
                    vision: backend.accepts_image_input(),
                    family: backend.compaction_snapcompact_family(),
                },
                &self.interrupt,
            );
        }

        if name == "session_search"
            && (self.experimental_context_owner() || self.exploration_owner())
        {
            return session::session_search_tool::call_owned(
                self.layout.as_ref(),
                &self.session,
                args,
            );
        }
        if name == "read"
            && !self.hermetic
            && let Some(path) = args["path"].as_str()
            && llm::images::may_reference_user_image(Path::new(path))
            && let Some(reference) =
                llm::images::authorized_user_reference(&self.session.messages(), Path::new(path))
        {
            if args["offset"].as_u64().is_some() {
                return denied("images do not support line offsets");
            }
            return match llm::images::verified_bytes(&reference) {
                Ok((bytes, _)) => ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: format!(
                        "Image: {path} ({} original bytes; explicitly supplied by the user; attached to the next model request when vision is available)",
                        bytes.len()
                    ),
                    is_error: false,
                },
                Err(error) => denied(&format!("Cannot read supplied image: {error:#}")),
            };
        }
        if name == "experience" {
            return self.experience_tool(backend, call, ui);
        }
        if name == "edit_preview" {
            return self.preview_tool(args, ui);
        }
        if matches!(name, "find" | "evidence") {
            match crate::agent::model::roles::retrieval_request(
                &self.workspace,
                name,
                args,
                &self.interrupt,
            ) {
                Ok(Some(request)) => return self.run_semantic_find(backend, args, request, ui),
                Ok(None) => {}
                Err(text) => {
                    return ToolOutput {
                        execution: Default::default(),
                        data: None,
                        text,
                        is_error: true,
                    };
                }
            }
        }
        if name == crate::storage::memory_backend::TOOL_NAME
            && self
                .memory_runtime
                .as_ref()
                .is_some_and(|memory| memory.offers_tool())
        {
            backend.release();
            return match self.selected_memory_tool(call, ui) {
                Ok(value) => ToolOutput {
                    execution: Default::default(),
                    text: value.to_string(),
                    data: Some(value),
                    is_error: false,
                },
                Err(error) => ToolOutput::err(format!("{error:#}")),
            };
        }
        if name == "bash"
            && let Some(shared) = self.shared_run_before(call)
        {
            // A peer's result is not this session's verification: no note_verification.
            backend.release();
            return shared;
        }
        if let Some(out) =
            self.run_background_tool_from(name, args, ui, Some(backend), provenance, read_reuse)
        {
            if name == "bash" {
                self.shared_run_after(&out);
                self.note_verification(call, &out, ui);
            }
            return out;
        }
        if name == "tts" {
            let env = crate::voice::VoiceEnv::from_config(self.config);
            return match crate::voice::tool::execute(&env, args, &self.interrupt) {
                Ok(text) => ToolOutput::structured(json!({"spoken":true,"result":text})),
                Err(error) => ToolOutput::err(format!("{error:#}")),
            };
        }
        if config::tools::BUILTIN.contains(&name) {
            let workspace = &self.workspace;
            let reuse = read_reuse.filter(|_| {
                name == "read"
                    && provenance == crate::agent::safety::approval::CallProvenance::Structured
                    && !["pre_tool_use", "post_tool_use"].iter().any(|event| {
                        crate::agent::safety::hooks::wanted_for_tool(self.tool_hooks(), event, name)
                    })
            });
            let out = blocking_with_heartbeat(ui, || match reuse {
                Some(reuse) => reuse.execute(workspace, args, |effective| {
                    tools::execute(workspace, name, effective, 120)
                }),
                None => tools::execute(workspace, name, args, 120),
            })
            .unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")));
            if name == "bash" {
                self.note_verification(call, &out, ui);
            }
            return out;
        }
        if name == crate::security::tool::NAME && self.ext.get(name).is_some() {
            let session_id = self.session.id().to_owned();
            let event = |event: &str, is_error| LifecycleEvent {
                event: event.into(),
                session_id: Some(session_id.clone()),
                turn_id: None,
                tool_name: Some(call.name.clone()),
                is_error,
            };
            self.ext.fire_lifecycle(&event("tool_started", None));
            let output = self.run_security_scan(backend, args, ui);
            self.ext
                .fire_lifecycle(&event("tool_finished", Some(output.is_error)));
            return output;
        }
        if let Some(tool) = self.ext.get(name) {
            let event = |event: &str, is_error| LifecycleEvent {
                event: event.into(),
                session_id: Some(self.session.id().to_owned()),
                turn_id: None,
                tool_name: Some(call.name.clone()),
                is_error,
            };
            self.ext.fire_lifecycle(&event("tool_started", None));
            // Network, LSP and MCP calls can take seconds; the UI keeps responding meanwhile.
            let workspace = &self.workspace;
            let config = self.effective_config();
            let output = blocking_with_heartbeat(ui, || {
                tool.call(
                    &ToolContext {
                        workspace,
                        config,
                        mutation_authorizer: None,
                        confirm: None,
                    },
                    args,
                )
            })
            .unwrap_or_else(|error| ToolOutput::err(format!("{error:#}")));
            self.ext
                .fire_lifecycle(&event("tool_finished", Some(output.is_error)));
            return output;
        }
        match name {
            "todo" => {
                if let Some(legacy) = crate::agent::workflow::todo::state_call(args) {
                    let contract = crate::agent::workflow::evidence::working_schema();
                    if let Err(error) =
                        crate::tools::surface::catalog::validate_arguments(&contract, &legacy)
                    {
                        return ToolOutput::err(format!(
                            "todo {}: {error}\nState contract: {}",
                            args["op"].as_str().unwrap_or_default(),
                            contract["function"]["description"]
                                .as_str()
                                .unwrap_or_default()
                        ));
                    }
                    return self.working_state_tool(&legacy, ui);
                }
                let mut next = self.todo.clone();
                let output = next.call(args);
                if output.is_error || next == self.todo {
                    return output;
                }
                let mut state = self.session.control_state();
                state["todo"] = next.snapshot();
                if let Err(error) = self.session.set_control_state(state) {
                    return denied(&format!("could not persist todos: {error:#}"));
                }
                self.todo = next;
                ui.todo(&self.todo);
                output
            }
            "goal" => match self.goal_tool(args) {
                Ok(text) => ToolOutput {
                    execution: Default::default(),
                    text,
                    is_error: false,
                    data: None,
                },
                Err(error) => denied(&error),
            },
            crate::agent::verify::verify_tool::NAME => self.verify_tool(args, ui),
            "revert_edit" => self.revert_edit_tool(call),
            "finish" => {
                let finish = match crate::agent::verify::completion::Finish::parse(args) {
                    Ok(finish) => finish,
                    Err(error) => return denied(&error),
                };
                if finish.outcome == TurnOutcome::Completed && !self.hermetic {
                    match self.wait_for_background_event(ui) {
                        Ok(true) => {
                            return ToolOutput::ok("Completion deferred: running work produced an event, new input arrived, or a stop boundary was reached. Incorporate worker findings and current user direction; fetch background job output or its artifact if only a terminal notice was delivered. Waiting does not establish success.".into())
                                .with_effects(crate::tools::surface::outcome::Effects::None);
                        }
                        Ok(false) => {}
                        Err(error) => {
                            return ToolOutput::err(format!("waiting for running work: {error:#}"))
                                .with_effects(crate::tools::surface::outcome::Effects::None);
                        }
                    }
                }

                match self.completion_decision(finish.outcome, &finish.summary, ui) {
                    crate::agent::verify::completion::Decision::Continue(note) => {
                        return ToolOutput::ok(note)
                            .with_effects(crate::tools::surface::outcome::Effects::None);
                    }
                    crate::agent::verify::completion::Decision::Incomplete(reason) => {
                        self.step_checks.finish_accepted = true;
                        return ToolOutput::structured(json!({
                            "accepted_outcome": "incomplete", "candidate_summary":finish.summary,
                            "reason":reason,
                        }));
                    }
                    crate::agent::verify::completion::Decision::Accept(_) => {}
                }
                if finish.outcome == TurnOutcome::NeedsInput {
                    let question = finish.question.as_deref().unwrap_or_default();
                    return match ui.ask_user(&finish.summary, question, &finish.options) {
                        Some(answer) if !answer.trim().is_empty() => ToolOutput {
                            execution: Default::default(),
                            data: None,
                            text: format!("User response: {answer}"),
                            is_error: false,
                        },
                        _ if !ui.interactive_input() => {
                            self.step_checks.finish_accepted = true;
                            ToolOutput {
                                execution: Default::default(),
                                data: None,
                                text: finish.text,
                                is_error: false,
                            }
                        }
                        _ => denied(
                            "The input request was cancelled; continue without it or ask again interactively.",
                        ),
                    };
                }

                self.step_checks.finish_accepted = true;
                ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: "Turn outcome accepted.".into(),
                    is_error: false,
                }
            }
            "tool_search" => {
                let query = match crate::tools::req_str(args, "query") {
                    Ok(query) => query,
                    Err(error) => return denied(&error),
                };
                let load = args.get("load").and_then(Value::as_bool) == Some(true);
                // `select:a,b` names exact tools: no ranking, every eligible name is a match.
                let selected: Option<Vec<String>> =
                    query.trim().strip_prefix("select:").map(|names| {
                        let mut names: Vec<String> = names
                            .split(',')
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned)
                            .collect();
                        let mut seen = std::collections::HashSet::new();
                        names.retain(|name| seen.insert(name.clone()));
                        names
                    });
                if selected
                    .as_ref()
                    .is_some_and(|names| names.is_empty() || names.len() > 32)
                {
                    return denied("`select:` takes 1 to 32 comma-separated exact tool names");
                }
                let listing = match selected {
                    Some(_) => None,
                    None => match listing_request(query) {
                        Ok(listing) => listing,
                        Err(error) => return denied(&error),
                    },
                };
                // Discovery covers every configured server, so it waits for any still connecting.
                self.ext.adopt_mcp(true);
                let mut descriptions = HashMap::new();
                let candidates: Vec<(String, String)> = self
                    .eligible_candidates()
                    .into_iter()
                    .map(|(name, description, indexed)| {
                        descriptions.insert(name.clone(), description);
                        (name, indexed)
                    })
                    .collect();
                if let Some(page) = listing {
                    // Listing is a read-only view: it never calls `activate_tools`.
                    let offered = self.offered();
                    return match listing_output(page, &descriptions, &offered) {
                        Ok(output) => {
                            listing_readiness(output, self.workspace.root(), self.config, &self.ext)
                        }
                        Err(error) => denied(&error),
                    };
                }
                let mut unknown = Vec::new();
                let matches = match &selected {
                    Some(names) => names
                        .iter()
                        .filter_map(|name| {
                            let known = candidates.iter().any(|(candidate, _)| candidate == name)
                                && !matches!(name.as_str(), "tool_search" | "tool_load");
                            if !known {
                                unknown.push(name.clone());
                            }
                            known.then(|| (name.clone(), String::new(), true))
                        })
                        .collect(),
                    None => crate::agent::model::assistance::discover_with_priority(
                        query,
                        candidates.clone(),
                        &|name| self.tool_readiness(name).routing_priority(),
                    ),
                };
                if matches.is_empty() && selected.is_none() {
                    // A ranking miss is not evidence that the capability is absent. Return the
                    // closest partial matches and the same bounded eligible catalog page as `*`,
                    // without activating anything.
                    let partial = crate::agent::model::assistance::partial_matches(
                        query,
                        candidates,
                        &|name| self.tool_readiness(name).routing_priority(),
                    );
                    let offered = self.offered();
                    let partial: Vec<Value> = partial
                        .into_iter()
                        .map(|name| {
                            let description: String = descriptions
                                .get(&name)
                                .map(|text| text.chars().take(TOOL_LIST_DESCRIPTION_CHARS).collect())
                                .unwrap_or_default();
                            let loaded = offered.contains(&name);
                            let (status, next) = activation_status(loaded, None);
                            let mut entry = json!({"name":name,"description":description,"status":status,"enabled_next_round":loaded,"next":next});
                            discovery_readiness(&mut entry, self.workspace.root(), self.config, &self.ext);
                            entry
                        })
                        .collect();
                    let reason = if partial.is_empty() {
                        "No ranked match; inspect the eligible catalog before concluding a capability is unavailable. Listing grants no permission and loads no tools."
                    } else {
                        "No tool matched most query terms; `search_matches` are the closest partial matches. Inspect them and the catalog before concluding a capability is unavailable, load one with tool_search names:[exact name], or search again with fewer, specific words. Matches and listing grant no permission and load no tools."
                    };
                    return match listing_output(0, &descriptions, &offered) {
                        Ok(mut output) => {
                            let mut page: Value = serde_json::from_str(&output.text)
                                .expect("listing_output serializes a JSON object");
                            page["search_matches"] = json!(partial);
                            page["fallback_reason"] = json!(reason);
                            output.text = page.to_string();
                            listing_readiness(output, self.workspace.root(), self.config, &self.ext)
                        }
                        Err(error) => denied(&error),
                    };
                }
                let activated: Vec<String> = matches
                    .iter()
                    .filter(|entry| entry.2)
                    .map(|entry| entry.0.clone())
                    .collect();
                let offered_before = self.offered();
                if !self.stable_tool_offer()
                    && selected.is_none()
                    && let Err(error) = self.activate_tools(&activated)
                {
                    return denied(&format!("persisting discovered tools: {error:#}"));
                }
                // `load` selects the exact or top matches through the `tool_load` owner, with its
                // eligibility checks, so a specialist costs one round instead of two.
                if load {
                    let offered = self.offered();
                    let pending: Vec<String> = activated
                        .iter()
                        .filter(|name| !offered.contains(name))
                        .cloned()
                        .collect();
                    if !pending.is_empty()
                        && let Err(error) = self.select_tools(&pending, &[])
                    {
                        return denied(&error);
                    }
                }
                let offered = self.offered();
                let mut matches: Vec<Value> = matches
                    .into_iter()
                    .map(|(name, _, _)| {
                        let description = descriptions.remove(&name).unwrap_or_default();
                        let loaded = offered.contains(&name);
                        // Each entry states whether it is callable on the next request, and the exact
                        // action that makes it callable. Discovery never grants permission itself.
                        let (status, next) =
                            activation_status(loaded, Some(offered_before.contains(&name)));
                        let parameters = self
                            .schemas(std::slice::from_ref(&name))
                            .ok()
                            .and_then(|mut schemas| schemas.pop())
                            .map(|schema| super::tool_exposure::parameter_summary(&schema))
                            .unwrap_or_default();
                        let mut entry = json!({"name":name,"description":description,"parameters":parameters,"status":status,"enabled_next_round":loaded,"next":next});
                        discovery_readiness(&mut entry, self.workspace.root(), self.config, &self.ext);
                        entry
                    })
                    .collect();
                // A load that made every match callable can be shown as the loaded definitions.
                let shown: Vec<String> = matches
                    .iter()
                    .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
                    .collect();
                if load
                    && unknown.is_empty()
                    && matches
                        .iter()
                        .all(|entry| entry["enabled_next_round"] == true)
                    && let Err(error) = self.note_tool_references(&call.id, &shown)
                {
                    return denied(&format!("persisting tool references: {error:#}"));
                }
                matches.extend(unknown.into_iter().map(|name| {
                    json!({"name":name,"status":"unavailable","enabled_next_round":false,"next":"not an eligible tool name; search by capability or list the catalog with `*`"})
                }));
                ToolOutput {
                    execution: Default::default(),
                    data: None,
                    text: json!(matches).to_string(),
                    is_error: false,
                }
            }
            "tool_load" => {
                let mut requested: Vec<String> = args["names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|name| crate::agent::run::tool_exposure::controller_name(name).to_owned())
                    .collect();
                let mut unload: Vec<String> = args["unload"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|name| crate::agent::run::tool_exposure::controller_name(name).to_owned())
                    .collect();
                requested.sort();
                requested.dedup();
                unload.sort();
                unload.dedup();
                if requested
                    .iter()
                    .chain(&unload)
                    .any(|name| !self.ext.contains(name) && self.ext.mcp_connecting().is_some())
                {
                    self.ext.adopt_mcp(true);
                }
                let offered_before = self.offered();
                if let Err(error) = self.select_tools(&requested, &unload) {
                    return denied(&error);
                }
                let offered = self.offered();
                let mut entries = Vec::new();
                let mut effective = Vec::new();
                let mut deferred = Vec::new();
                for (names, action) in [(&requested, "load"), (&unload, "unload")] {
                    for name in names {
                        let loaded = offered.contains(name);
                        let (status, next) =
                            activation_status(loaded, Some(offered_before.contains(name)));
                        let eligible = self.tool_eligible(name);
                        let next = if eligible {
                            next
                        } else {
                            "unavailable under the current mode or tool allowlist; loading cannot bypass this boundary"
                        };
                        let mut entry = json!({
                            "name": name,
                            "action": action,
                            "eligible": eligible,
                            "status": status,
                            "enabled_next_round": loaded,
                            "retained_by_policy": action == "unload" && loaded,
                            "next": next,
                        });
                        discovery_readiness(
                            &mut entry,
                            self.workspace.root(),
                            self.config,
                            &self.ext,
                        );
                        entries.push(entry);
                        if loaded {
                            effective.push(name.as_str());
                        } else {
                            deferred.push(name.as_str());
                        }
                    }
                }
                // A load that made every requested tool callable can be shown as the loaded
                // definitions on a route that defers tools; any unload keeps the text.
                if unload.is_empty()
                    && deferred.is_empty()
                    && let Err(error) = self.note_tool_references(&call.id, &requested)
                {
                    return denied(&format!("persisting tool references: {error:#}"));
                }
                ToolOutput {
                    execution: Default::default(),
                    data: Some(json!({"selection_persisted": true, "tools": entries})),
                    text: format!(
                        "Selection persisted. Next request: offered [{}]; not offered [{}]. Unload removes explicit activation, but policy retains core tools. Loading grants no permission and does not enable calls in this batch.",
                        effective.join(", "),
                        deferred.join(", ")
                    ),
                    is_error: false,
                }
            }
            "skill" | "skill_manage" | "session_search" => match &self.layout {
                None => denied("unavailable here"),
                Some(layout) => match name {
                    "skill"
                        if args
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|skill| !self.modes.launch.skill_visible(skill)) =>
                    {
                        crate::runtime::tool_output::ToolOutput {
                            execution: Default::default(),
                            data: None,
                            text: "skill is withheld by the --skills launch allowlist".into(),
                            is_error: true,
                        }
                    }
                    "skill" => skills::skill_tool::call(layout, args),
                    "skill_manage" => skills::manage_tool::call(layout, args),
                    _ if crate::tools::present(args, "history")
                        || crate::tools::present(args, "entry_id")
                        || crate::tools::present(args, "all_branches") =>
                    {
                        denied(
                            "raw session history requires the experimental durable session owner",
                        )
                    }
                    _ => session::session_search_tool::call(layout, args),
                },
            },
            "task" => self.run_task(backend, args, ui),
            "side_request" => self.side_question_with_input(args, &mut || ui.pending_input()),
            "message" => self.message_tool(args),
            "peers" => self.peer_coordination(args),
            "solve" => self.run_solve(backend, args, ui),
            "escalate" => {
                backend.release();
                let env = crate::agent::workers::subagents::Env {
                    config: &self.auth_config,
                    workspace: &self.workspace,
                    ext: &self.ext,
                    layout: self.layout.as_ref(),
                    harness: &self.version.harness,
                    interrupt: &self.interrupt,
                    usage: Some(&self.usage_scope),
                    context_cap: Some(self.context_window()),
                    agents_messages: &self.agents_messages,
                    plan_mode: self.plan_mode,
                    memory: self.memory_runtime.as_ref(),
                };
                crate::agent::workflow::solve::escalate(
                    env,
                    args,
                    &mut self.session_cost,
                    ui,
                    self.session.id(),
                    self.hermetic,
                )
            }
            other => match self.config.tools.get(other) {
                Some(tool) => custom_tool_output(tool, args, &self.interrupt),
                None if self.ext.is_retired_mcp(other) => denied(&format!(
                    "MCP tool `{other}` was removed by its server; use the tools currently offered"
                )),
                None => denied(&format!("unknown tool: {other}")),
            },
        }
    }
    pub(in crate::agent) fn intercept_native_effect(
        &mut self,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
        call: &ToolCall,
        ui: &mut dyn Ui,
    ) -> std::result::Result<(), String> {
        let intercepted = gate.intercept(self.workspace.root(), call);
        let state_changed = gate.take_state_changed();
        self.portable_callback_intercepted |= gate.blocked().is_some();
        if intercepted.is_some() || state_changed {
            let mut control = self.session.control_state();
            gate.state().save(&mut control);
            if let Err(error) = self.session.set_control_state(control) {
                self.portable_callback_intercepted = true;
                return Err(gate.journaling_failed(format!(
                    "journaling portable native-effect state before execution: {error:#}; this effect and remaining native callbacks were refused"
                )));
            }
        }
        if let Some(reason) = intercepted {
            ui.notice(&reason);
            return Err(reason);
        }
        Ok(())
    }
    pub(in crate::agent) fn code_callback(
        &mut self,
        backend: &mut Backend,
        request: crate::tools::integrations::ext::ToolCallback,
        owner: &str,
        ui: &mut dyn Ui,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
        provenance: crate::agent::safety::approval::CallProvenance,
    ) -> ToolOutput {
        self.code_callback_mode(
            backend,
            request,
            ui,
            gate,
            CallbackSource {
                owner,
                provenance,
                mode: DispatchMode::Foreground,
            },
        )
    }

    fn code_callback_mode(
        &mut self,
        backend: &mut Backend,
        request: crate::tools::integrations::ext::ToolCallback,
        ui: &mut dyn Ui,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
        source: CallbackSource<'_>,
    ) -> ToolOutput {
        if self.interrupt.load(Ordering::SeqCst) {
            return interrupted_tool();
        }
        if let Some(reason) = gate.blocked() {
            return ToolOutput::err(reason);
        }
        let mut names = self.offered();
        names.retain(|name| {
            !matches!(
                name.as_str(),
                "finish" | "exit_plan" | "checkpoint" | "rewind"
            ) && !self
                .ext
                .get(name)
                .is_some_and(|tool| tool.supports_tool_dispatch())
        });
        let (name, arguments) = match request {
            crate::tools::integrations::ext::ToolCallback::Schemas => {
                return match self.schemas(&names) {
                    Ok(schemas) => ToolOutput {
                        execution: Default::default(),
                        data: Some(Value::Array(schemas)),
                        text: String::new(),
                        is_error: false,
                    },
                    Err(error) => {
                        ToolOutput::err(format!("listing authorized code-mode tools: {error:#}"))
                    }
                };
            }
            crate::tools::integrations::ext::ToolCallback::Call { name, arguments } => {
                // A consolidated member's own name still works in code: it runs as the successor
                // call that reaches it, under the successor's scope and approval.
                match crate::tools::surface::facade::successor_call(&name, &arguments) {
                    Some((successor, call))
                        if self.ext.is_folded(&name) && names.iter().any(|n| n == successor) =>
                    {
                        (successor.to_owned(), call)
                    }
                    _ => (name, arguments),
                }
            }
        };
        if name == crate::tools::exec::eval::BATCH_CALLBACK {
            return self.code_callback_batch(backend, &arguments, &names, ui, gate, source);
        }
        if !names.contains(&name) {
            return ToolOutput::err(format!(
                "tool {name:?} is not available in this code-mode scope"
            ));
        }
        let schemas = match self.schemas(std::slice::from_ref(&name)) {
            Ok(schemas) => schemas,
            Err(error) => {
                return ToolOutput::err(format!("resolving code-mode tool schema: {error:#}"));
            }
        };
        let call = ToolCall {
            id: String::new(),
            name,
            arguments,
            malformed_arguments: false,
        };
        if let Some((_, why)) = call_form_error(&call, &schemas, &names) {
            return ToolOutput::err(format!("invalid code-mode tool call: {why}"));
        }
        self.execute_callback_from(backend, ui, &call, &schemas, source, gate)
    }

    /// `tool.batch([...])` from a code cell. Consecutive entries that are offered, hook-free,
    /// rule-permitted parallel reads run together through the ordinary parallel-read group with
    /// one coalesced journal flush on each side; every other entry is an ordinary serial callback
    /// with its own admission and approval, and closes the read group before it. Results keep
    /// entry order, and each group is chosen only after the entries before it settled.
    fn code_callback_batch(
        &mut self,
        backend: &mut Backend,
        arguments: &Value,
        names: &[String],
        ui: &mut dyn Ui,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
        source: CallbackSource<'_>,
    ) -> ToolOutput {
        let calls = match code_batch_calls(arguments, source.owner) {
            Ok(calls) => calls,
            Err(refusal) => return ToolOutput::err(refusal),
        };
        let mut outputs = Vec::with_capacity(calls.len());
        let mut index = 0;
        while index < calls.len() {
            let group = calls[index..]
                .iter()
                .take_while(|call| self.shared_callback_read(call, names))
                .count();
            // A lone read inside a mixed batch keeps the ordinary serial callback path.
            if group > 1 || group == calls.len() {
                outputs.extend(self.code_callback_reads(
                    &calls[index..index + group],
                    names,
                    ui,
                    gate,
                    source.owner,
                ));
                index += group;
                continue;
            }
            let call = calls[index].clone();
            index += 1;
            outputs.push(if call.name == crate::tools::exec::eval::BATCH_CALLBACK {
                ToolOutput::err("tool.batch entries cannot nest tool.batch")
            } else {
                self.code_callback_mode(
                    backend,
                    crate::tools::integrations::ext::ToolCallback::Call {
                        name: call.name,
                        arguments: call.arguments,
                    },
                    ui,
                    gate,
                    source.clone(),
                )
            });
        }
        code_batch_output(outputs)
    }

    /// Whether one `tool.batch` entry may join a shared read group at this point in the batch.
    fn shared_callback_read(&self, call: &ToolCall, names: &[String]) -> bool {
        self.config.eval.parallel_batches
            && call.name != crate::tools::exec::eval::BATCH_CALLBACK
            && names.contains(&call.name)
            && crate::agent::safety::approval::parallel_read(&self.ext, &call.name, &call.arguments)
            && ["pre_tool_use", "post_tool_use"].iter().all(|event| {
                !crate::agent::safety::hooks::wanted_for_tool(self.tool_hooks(), event, &call.name)
            })
            && !crate::agent::safety::approval::denied_by_rule(
                &self.always,
                call,
                self.workspace.root(),
            )
            && (self.pending_check.is_none() || call.name != "bash")
            && image_question(call).is_none()
    }

    /// Admitted read-only batch entries: per-entry form checks and portable-rule interception,
    /// then the parallel-read group. Reads need no approval; their records share two flushes.
    fn code_callback_reads(
        &mut self,
        calls: &[ToolCall],
        names: &[String],
        ui: &mut dyn Ui,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
        owner: &str,
    ) -> Vec<ToolOutput> {
        let mut outputs: Vec<Option<ToolOutput>> = vec![None; calls.len()];
        let mut admitted = Vec::new();
        let mut positions = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            if self.interrupt.load(Ordering::SeqCst) {
                outputs[index] = Some(interrupted_tool());
                continue;
            }
            if let Some(reason) = gate.blocked() {
                outputs[index] = Some(ToolOutput::err(reason));
                continue;
            }
            let schemas = match self.schemas(std::slice::from_ref(&call.name)) {
                Ok(schemas) => schemas,
                Err(error) => {
                    outputs[index] = Some(ToolOutput::err(format!(
                        "resolving code-mode tool schema: {error:#}"
                    )));
                    continue;
                }
            };
            if let Some((_, why)) = call_form_error(call, &schemas, names) {
                outputs[index] = Some(ToolOutput::err(format!(
                    "invalid code-mode tool call: {why}"
                )));
                continue;
            }
            if let Err(reason) = self.intercept_native_effect(gate, call, ui) {
                outputs[index] = Some(ToolOutput::err(reason));
                continue;
            }
            if let Err(error) = self.record(
                self.cur_step,
                "code_tool_call",
                json!({"parent":owner,"name":call.name,"input":call.arguments,"contract":tools::surface::contracts::identities(&schemas,&self.ext).get(&call.name)}),
            ) {
                outputs[index] = Some(ToolOutput::err(format!(
                    "recording code-mode call before execution: {error:#}"
                )));
                continue;
            }
            admitted.push(call.clone());
            positions.push(index);
        }
        if !admitted.is_empty() {
            let mut steering = Vec::new();
            let results = parallel_tools_with_reads(
                ParallelToolContext {
                    workspace: &self.workspace,
                    config: self.effective_config(),
                    ext: &self.ext,
                    interrupt: &self.interrupt,
                    ui,
                    pending_steering: &mut steering,
                },
                &admitted,
                None,
                None,
                None,
            );
            self.pending_tool_steering.extend(steering);
            let flushed = self.session.flush_runtime();
            for ((call, (mut output, _)), position) in admitted.iter().zip(results).zip(positions) {
                modes::lifecycle::Lifecycle::dispatched(self, call, &output);
                let image = match callback_image(&self.workspace, call, &mut output, None) {
                    Ok(image) => image,
                    Err(error) => {
                        output.is_error = true;
                        output.text.push_str(&format!(
                            "\n[Preparing callback image evidence failed: {error:#}]"
                        ));
                        None
                    }
                };
                if let Some((reference, _, _)) = &image {
                    ui.image(reference);
                }
                let recorded = flushed.as_ref().map_err(|error| anyhow::anyhow!("{error:#}")).and_then(|_| {
                    self.record(
                        self.cur_step,
                        "code_tool_result",
                        json!({"parent":owner,"name":call.name,"is_error":output.is_error,"output":output.text,"data":output.data,"execution":output.execution}),
                    )
                });
                if let Err(error) = recorded {
                    output.execution.effects = crate::tools::surface::outcome::Effects::Unknown;
                    output.is_error = true;
                    output.text.push_str(&format!("\n[Recording completed callback evidence failed; inspect state before retrying: {error:#}]"));
                }
                if let Err(error) = callback_image_payload(&mut output, image) {
                    output.is_error = true;
                    output.text.push_str(&format!(
                        "\n[Encoding callback image payload failed: {error:#}]"
                    ));
                }
                outputs[position] = Some(output);
            }
        }
        outputs
            .into_iter()
            .map(|output| output.expect("one result per batch entry"))
            .collect()
    }

    fn execute_callback_from(
        &mut self,
        backend: &mut Backend,
        ui: &mut dyn Ui,
        call: &ToolCall,
        schemas: &[Value],
        source: CallbackSource<'_>,
        gate: &mut crate::agent::safety::rules::CallbackGate<'_>,
    ) -> ToolOutput {
        let CallbackSource {
            owner,
            provenance,
            mode,
        } = source;
        if self.interrupt.load(Ordering::SeqCst) {
            return interrupted_tool();
        }
        if let Some(reason) = gate.blocked() {
            return ToolOutput::err(reason);
        }
        if let Err(reason) = self.intercept_native_effect(gate, call, ui) {
            return ToolOutput::err(reason);
        }
        if let Err(error) = self.record(
            self.cur_step,
            "code_tool_call",
            json!({"parent":owner,"name":call.name,"input":call.arguments,"contract":tools::surface::contracts::identities(schemas,&self.ext).get(&call.name)}),
        ) {
            return ToolOutput::err(format!(
                "recording code-mode call before execution: {error:#}"
            ));
        }
        ui.tool_call_with_args(&call.id, &call.name, &summarize(call), &call.arguments);
        // Effects need their write-ahead record durable first. A read-only callback has no effect
        // to recover, so its call record is flushed with its result instead (one flush, not two).
        if !crate::agent::safety::approval::parallel_read(&self.ext, &call.name, &call.arguments)
            && let Err(error) = self.session.flush_runtime()
        {
            let output = ToolOutput::err(format!(
                "journaling code-mode call before execution: {error:#}"
            ));
            ui.tool_result_with_id(&call.id, &call.name, &output);
            return output;
        }
        let started = Instant::now();
        let mut output = self.dispatch_call_mode(backend, call, provenance, ui, mode, None);
        modes::lifecycle::Lifecycle::dispatched(self, call, &output);
        let supplied = if call.name == "read" {
            call.arguments["path"]
                .as_str()
                .filter(|path| llm::images::may_reference_user_image(Path::new(path)))
                .and_then(|path| {
                    llm::images::authorized_user_reference(
                        &self.session.messages(),
                        Path::new(path),
                    )
                })
        } else {
            None
        };
        let image = match callback_image(&self.workspace, call, &mut output, supplied) {
            Ok(image) => image,
            Err(error) => {
                output.is_error = true;
                output.text.push_str(&format!(
                    "\n[Preparing callback image evidence failed: {error:#}]"
                ));
                None
            }
        };
        if let Some((reference, _, _)) = &image {
            ui.image(reference);
        }
        ui.tool_timing(&call.id, &call.name, started.elapsed().as_secs_f64());
        ui.tool_result_with_id(&call.id, &call.name, &output);
        if call.name == "ask" {
            self.callback_question_results
                .push(json!({"parent":owner,"arguments":call.arguments,
                "text":output.text,"data":output.data,"is_error":output.is_error}));
        }
        // Persist compact image references, not duplicate base64 pixels, in the
        // execution ledger before returning the richer payload to native code.
        if let Err(error) = self.session.flush_runtime().and_then(|_| {
            self.record(
                self.cur_step,
                "code_tool_result",
                json!({"parent":owner,"name":call.name,"is_error":output.is_error,"output":output.text,"data":output.data,"execution":output.execution}),
            )
        }) {
            output.execution.effects = crate::tools::surface::outcome::Effects::Unknown;
            output.is_error = true;
            output.text.push_str(&format!("\n[Recording completed callback evidence failed; inspect state before retrying: {error:#}]"));
        }
        if let Err(error) = callback_image_payload(&mut output, image) {
            output.is_error = true;
            output.text.push_str(&format!(
                "\n[Encoding callback image payload failed: {error:#}]"
            ));
        }
        output
    }

    /// Bash jobs owned by this session that must settle before completion can be accepted.
    pub(in crate::agent) fn awaited_background_jobs(&self) -> Vec<String> {
        let mut jobs = Vec::new();
        for name in self.ext.names() {
            if let Some(tool) = self.ext.get(name) {
                jobs.extend(tool.awaited_work());
            }
        }
        jobs
    }

    /// Keep unfinished bash work visible without blocking the controller or user steering.
    pub(in crate::agent) fn await_background_jobs(&mut self, ui: &mut dyn Ui) -> bool {
        self.pump_subagents(ui);
        let jobs = self.awaited_background_jobs();
        let questions = self.side_questions.running_ids();
        if !questions.is_empty() && !self.interrupt.load(Ordering::Relaxed) {
            self.background_notices.push(format!("[Side questions still running]\n{}\nContinue independent work or use a bounded `task` action=question_wait; do not claim their answers are available yet.", questions.join("\n")));
        }
        let workers = self.live_tasks.running_ids();
        if !self.interrupt.load(Ordering::Relaxed) {
            if !jobs.is_empty() {
                self.background_notices.push(format!(
                    "[Background work is still running]\n{}\nContinue independent work first. If nothing useful remains, tell the user what is pending and use a bounded `bg wait`. Do not report these jobs as completed; inspect their results or stop jobs you no longer need.",
                    jobs.join("\n")
                ));
            }
            // Live workers are awaited like background jobs: the turn waits for their results rather than ending.
            if !workers.is_empty() {
                self.background_notices.push(format!(
                    "[Live workers are still running]\n{}\nWait for their results with `task wait`, or continue independent work first. Their findings arrive as they finish; do not report these workers as completed before their results are in.",
                    workers.join("\n")
                ));
            }
            if let Some(notice) = self.exit_bound_jobs_notice() {
                self.background_notices.push(notice);
            }
        }
        // Finished worker results are queued for the next request: the turn continues to incorporate them.
        !self.background_notices.is_empty()
            || !self.live_tasks.undelivered().is_empty()
            || self.side_questions.has_undelivered()
    }

    /// An unattended run (`Launch::unattended`) kills its session-owned `bg` jobs when it exits, so
    /// a service the task needs afterwards would silently disappear. Returns a notice naming the
    /// running jobs not yet reported, once per job; `None` outside unattended runs.
    pub(in crate::agent) fn exit_bound_jobs_notice(&mut self) -> Option<String> {
        if !self.modes.launch.unattended {
            return None;
        }
        let mut jobs = Vec::new();
        for name in self.ext.names() {
            if let Some(tool) = self.ext.get(name) {
                jobs.extend(tool.session_work());
            }
        }
        jobs.retain(|job| self.exit_bound_jobs_noticed.insert(job.clone()));
        (!jobs.is_empty()).then(|| format!(
            "[Background jobs end with this run]\n{}\nThis is an unattended run: when it ends, Dial kills these session jobs. If the task needs one after you finish (a server, index or service that a user or check will connect to), stop it with `bg kill`, start it again with `bg` action=start and persist=true (named; it survives this run until `lifetime`, default 3600 s), confirm it serves, then finish. Otherwise stop jobs you no longer need. Never report a session job as still running after the run.",
            jobs.join("\n")
        ))
    }
}

#[cfg(test)]
mod tests;
